use std::cell::Cell;
use std::cmp::Ordering as CmpOrd;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Mutex;

#[cfg(feature = "table_stat")]
use std::fmt::Display;

use crate::node::{Node, NodePtr};

// 全局粗粒度锁，保护整个唯一节点表
static TABLE_LOCK: once_cell::sync::Lazy<Mutex<()>> = once_cell::sync::Lazy::new(|| Mutex::new(()));

#[allow(dead_code)]
pub trait Set {
    // concurrent operations
    fn get_or_insert(&self, new_ptr: *mut Node) -> (*mut Node, bool);

    // single-threaded operations
    fn mark_nodes(&self) -> usize;
    fn gc_unmarked(&self);
    fn unmark_nodes(&self);
    fn grow(&self);

    fn sanity_check(&self);
}

#[cfg(feature = "table_stat")]
#[derive(Default)]
pub struct TableStat {
    pub unique_access: AtomicUsize,
    pub unique_chain: AtomicUsize,
    pub unique_hit: AtomicUsize,
    pub unique_miss: AtomicUsize,
}

#[cfg(feature = "table_stat")]
pub struct TableStatReport {
    pub unique_access: usize,
    pub unique_chain: usize,
    pub unique_hit: usize,
    pub unique_miss: usize,
    pub table_size: usize,
    pub node_count: usize,
    pub load_factor: f64,
    pub empty_buckets: usize,
    pub non_empty_buckets: usize,
    pub min_chain_len: usize,
    pub max_chain_len: usize,
    pub avg_non_empty_chain_len: f64,
    pub histogram: [usize; 9],
}

#[cfg(feature = "table_stat")]
impl Display for TableStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "unique_access: {}, ",
            self.unique_access.load(Ordering::Relaxed)
        ))?;
        f.write_fmt(format_args!(
            "unique_chain: {}, ",
            self.unique_chain.load(Ordering::Relaxed)
        ))?;
        f.write_fmt(format_args!(
            "unique_hit: {}, ",
            self.unique_hit.load(Ordering::Relaxed)
        ))?;
        f.write_fmt(format_args!(
            "unique_miss: {}",
            self.unique_miss.load(Ordering::Relaxed)
        ))?;
        Ok(())
    }
}

#[cfg(feature = "table_stat")]
impl Display for TableStatReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "unique_access: {}, unique_chain: {}, unique_hit: {}, unique_miss: {}",
            self.unique_access, self.unique_chain, self.unique_hit, self.unique_miss
        )?;
        writeln!(
            f,
            "table_size: {} buckets, node_count: {}, load_factor: {:.3}",
            self.table_size, self.node_count, self.load_factor
        )?;
        writeln!(
            f,
            "buckets: empty: {}, non_empty: {}",
            self.empty_buckets, self.non_empty_buckets
        )?;
        writeln!(
            f,
            "chain_len: min: {}, max: {}, avg_non_empty: {:.3}",
            self.min_chain_len, self.max_chain_len, self.avg_non_empty_chain_len
        )?;
        write!(
            f,
            "histogram: 0: {}, 1: {}, 2-3: {}, 4-7: {}, 8-15: {}, 16-31: {}, 32-63: {}, 64-127: {}, 128+: {}",
            self.histogram[0],
            self.histogram[1],
            self.histogram[2],
            self.histogram[3],
            self.histogram[4],
            self.histogram[5],
            self.histogram[6],
            self.histogram[7],
            self.histogram[8]
        )?;
        Ok(())
    }
}

pub struct LockFreeSet {
    buckets: Cell<*mut [AtomicPtr<Node>]>,
    size_exp: Cell<usize>,
    num_marks: Cell<usize>,
    num_entry: AtomicUsize,
    needs_gc: AtomicBool,
    #[cfg(feature = "table_stat")]
    pub stat: TableStat,
}

impl LockFreeSet {
    pub fn with_capacity(cap: usize) -> Self {
        let size_exp = cap.isolate_highest_one().trailing_zeros() + 1;
        let buckets = (0..(1 << size_exp))
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        LockFreeSet {
            buckets: Cell::new(Box::into_raw(buckets)),
            size_exp: Cell::new(size_exp as usize),
            num_marks: Cell::new(0),
            num_entry: AtomicUsize::new(0),
            needs_gc: AtomicBool::new(false),
            #[cfg(feature = "table_stat")]
            stat: TableStat::default(),
        }
    }

    pub fn bucket_size(&self) -> usize {
        1 << self.size_exp.get()
    }

    pub fn needs_gc(&self) -> bool {
        self.needs_gc.load(Ordering::Relaxed)
    }

    pub fn clear_needs_gc(&self) {
        self.needs_gc.store(false, Ordering::Relaxed);
    }

    pub fn entry_num(&self) -> usize {
        self.num_entry.load(Ordering::Relaxed)
    }

    const MARK: usize = 1 << (std::mem::size_of::<usize>() * 8 - 1);

    fn mark_rec(&self, root: *mut Node, counter: &mut usize) {
        let node_ref = unsafe { &*root };
        if (node_ref.level & (Self::MARK)) != 0 {
            // if already marked, then subtree marked
            return;
        }
        // mark
        unsafe {
            (*root).level |= Self::MARK;
        }
        *counter += 1;
        let low = node_ref.low;
        let high = node_ref.high;
        if !low.is_null() {
            self.mark_rec(low, counter);
        }
        if !high.is_null() {
            self.mark_rec(high, counter);
        }
    }

    #[cfg(feature = "table_stat")]
    pub fn table_stat_report(&self) -> TableStatReport {
        let size_exp = self.size_exp.get();
        let table_size = 1usize << size_exp;
        let buckets = unsafe { &*self.buckets.get() };
        let mut histogram = [0usize; 9];
        let mut empty_buckets = 0usize;
        let mut non_empty_buckets = 0usize;
        let mut min_chain_len = usize::MAX;
        let mut max_chain_len = 0usize;
        let mut node_count = 0usize;

        for i in 0..table_size {
            let mut len = 0usize;
            let mut curr = buckets[i].load(Ordering::Relaxed);
            while !curr.is_null() {
                len += 1;
                curr = unsafe { (*curr).next.load(Ordering::Relaxed) };
            }

            node_count += len;

            if len == 0 {
                empty_buckets += 1;
                histogram[0] += 1;
                continue;
            }

            non_empty_buckets += 1;
            if len < min_chain_len {
                min_chain_len = len;
            }
            if len > max_chain_len {
                max_chain_len = len;
            }

            match len {
                1 => histogram[1] += 1,
                2..=3 => histogram[2] += 1,
                4..=7 => histogram[3] += 1,
                8..=15 => histogram[4] += 1,
                16..=31 => histogram[5] += 1,
                32..=63 => histogram[6] += 1,
                64..=127 => histogram[7] += 1,
                _ => histogram[8] += 1,
            }
        }

        if non_empty_buckets == 0 {
            min_chain_len = 0;
        }

        let load_factor = if table_size > 0 {
            node_count as f64 / table_size as f64
        } else {
            0.0
        };

        let avg_non_empty_chain_len = if non_empty_buckets > 0 {
            node_count as f64 / non_empty_buckets as f64
        } else {
            0.0
        };

        TableStatReport {
            unique_access: self.stat.unique_access.load(Ordering::Relaxed),
            unique_chain: self.stat.unique_chain.load(Ordering::Relaxed),
            unique_hit: self.stat.unique_hit.load(Ordering::Relaxed),
            unique_miss: self.stat.unique_miss.load(Ordering::Relaxed),
            table_size,
            node_count,
            load_factor,
            empty_buckets,
            non_empty_buckets,
            min_chain_len,
            max_chain_len,
            avg_non_empty_chain_len,
            histogram,
        }
    }
}

impl Set for LockFreeSet {
    // 链表只增长、不删除节点（因此读到的指针永远有效，不会被释放）；
    //节点插入后，level/low/high 不再改变；
    // ptr 指向的节点是有效的、且当前不在链表中；
    // 用 (level, low, high) 作为“值相等”的判定，并按这个 key 升序插入（保证并发下不会插入重复值：第二个线程 CAS 失败后会重试并看到已存在节点）。
    fn get_or_insert(&self, new_ptr: *mut Node) -> (*mut Node, bool) {
        let idx = new_ptr.node_hash() & ((1 << self.size_exp.get()) - 1);

        // 获取全局粗粒度锁
        let _lock = TABLE_LOCK.lock().unwrap();

        let head = &(unsafe { &*self.buckets.get() })[idx as usize];
        let new_key = unsafe { &*new_ptr }.key();

        // 在锁保护下遍历链表
        let mut prev: &AtomicPtr<Node> = head;
        let mut curr: *mut Node = prev.load(Ordering::Acquire);

        while !curr.is_null() {
            let curr_ref = unsafe { &*curr };
            let curr_key = curr_ref.key();

            match curr_key.cmp(&new_key) {
                CmpOrd::Equal => {
                    return (curr, false);
                }
                CmpOrd::Greater => {
                    break;
                }
                CmpOrd::Less => {
                    prev = &curr_ref.next;
                    curr = prev.load(Ordering::Acquire);
                }
            }
        }

        // 插入新节点（无需 CAS，因为有锁保护）
        unsafe {
            (*new_ptr).next.store(curr, Ordering::Relaxed);
        }
        prev.store(new_ptr, Ordering::Release);

        let prev_count = self.num_entry.fetch_add(1, Ordering::Relaxed);
        if prev_count + 1 >= 1 << self.size_exp.get() {
            self.needs_gc.store(true, Ordering::Relaxed);
        }
        (new_ptr, true)
    }

    // 倍增大小，重新哈希所有节点，保持每个桶内的升序
    fn grow(&self) {
        let new_size_exp = self.size_exp.get() + 1;
        let new_buckets = (0..(1 << new_size_exp))
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let old_buckets = unsafe { &*self.buckets.get() };
        for i in 0..(1 << self.size_exp.get()) {
            let mut curr = old_buckets[i].load(Ordering::Relaxed);
            while !curr.is_null() {
                let next = unsafe { (*curr).next.load(Ordering::Relaxed) };
                let idx = curr.node_hash() & ((1 << new_size_exp) - 1);
                // 插入到 new_buckets[idx] 中，保持升序
                let head = &new_buckets[idx as usize];
                let mut prev: &AtomicPtr<Node> = head;
                let mut scan: *mut Node = prev.load(Ordering::Acquire);
                let curr_key = unsafe { &*curr }.key();
                while !scan.is_null() {
                    let scan_key = unsafe { &*scan }.key();
                    if scan_key.cmp(&curr_key) == CmpOrd::Greater {
                        break;
                    }
                    prev = unsafe { &(*scan).next };
                    scan = prev.load(Ordering::Acquire);
                }
                // 插入 curr 到 prev -> scan 之间
                unsafe {
                    (*curr).next.store(scan, Ordering::Relaxed);
                }
                prev.store(curr, Ordering::Release);
                curr = next;
            }
        }
        // 回收旧桶数组
        let old_buckets_box = unsafe { Box::from_raw(self.buckets.get()) };
        drop(old_buckets_box);
        // 切换到新桶数组
        self.buckets.set(Box::into_raw(new_buckets));
        self.size_exp.set(new_size_exp);
        self.needs_gc.store(false, Ordering::Relaxed);
    }

    // Starting from nodes that its rec_cnt != 0, mark recursively by setting the highest of
    // `level` to 1.
    fn mark_nodes(&self) -> usize {
        let mut counter = 0usize;
        for i in 0..(1 << self.size_exp.get()) {
            let head = unsafe { &*self.buckets.get() }[i].load(Ordering::Relaxed);
            let mut curr = head;
            while !curr.is_null() {
                let curr_ref = unsafe { &*curr };
                if curr_ref.ref_cnt.load(Ordering::Relaxed) != 0
                    && curr_ref.level & (Self::MARK) == 0
                {
                    self.mark_rec(curr, &mut counter);
                }
                curr = curr_ref.next.load(Ordering::Relaxed);
            }
        }
        debug_assert!(counter <= (2 << self.size_exp.get()));
        self.num_marks.set(counter);
        counter
    }

    // Garbage collect unmarked nodes in a single-threaded manner, no CAS is used.
    fn gc_unmarked(&self) {
        for i in 0..(1 << self.size_exp.get()) {
            let head = unsafe { &*self.buckets.get() }[i].load(Ordering::Relaxed);
            let mut prev: &AtomicPtr<Node> = &unsafe { &*self.buckets.get() }[i];
            let mut curr = head;
            while !curr.is_null() {
                let curr_ref = unsafe { &*curr };
                if (curr_ref.level & (Self::MARK)) == 0 {
                    // unmarked: remove from the list
                    let next = curr_ref.next.load(Ordering::Relaxed);
                    prev.store(next, Ordering::Relaxed);
                    // free the node
                    unsafe {
                        drop(Box::from_raw(curr));
                    }
                    curr = next;
                } else {
                    // marked: keep it
                    prev = &curr_ref.next;
                    curr = curr_ref.next.load(Ordering::Relaxed);
                }
            }
        }
        self.num_entry
            .store(self.num_marks.get(), Ordering::Relaxed);
        self.needs_gc.store(false, Ordering::Relaxed);
    }

    fn unmark_nodes(&self) {
        for i in 0..(1 << self.size_exp.get()) {
            let head = unsafe { &*self.buckets.get() }[i].load(Ordering::Relaxed);
            let mut curr = head;
            while !curr.is_null() {
                let curr_ref = unsafe { &*curr };
                if (curr_ref.level & (Self::MARK)) != 0 {
                    // unmark
                    unsafe {
                        (*curr).level &= !Self::MARK;
                    }
                }
                curr = curr_ref.next.load(Ordering::Relaxed);
            }
        }
    }

    fn sanity_check(&self) {
        // check that every entry is correctly placed in the bucket
        // check that every bucket is sorted
        // check that there is no duplicate entry
        for i in 0..(1 << self.size_exp.get()) {
            let head = unsafe { &*self.buckets.get() }[i].load(Ordering::Relaxed);
            let mut curr = head;
            let mut prev_key: Option<(usize, *mut Node)> = None;
            while !curr.is_null() {
                let curr_ref = unsafe { &*curr };
                let curr_key = curr_ref.key();
                let idx = curr.node_hash() & ((1 << self.size_exp.get()) - 1);
                assert_eq!(idx as usize, i, "Node in wrong bucket");
                if let Some((prev_level, prev_ptr)) = prev_key {
                    assert!(
                        curr_key > unsafe { &*prev_ptr }.key(),
                        "Bucket not sorted or duplicate entry"
                    );
                }
                prev_key = Some((curr_ref.level, curr));
                curr = curr_ref.next.load(Ordering::Relaxed);
            }
        }
    }
}

impl Drop for LockFreeSet {
    fn drop(&mut self) {
        let buckets = unsafe { Box::from_raw(self.buckets.get()) };
        for bucket in buckets.iter() {
            let mut curr = bucket.load(Ordering::Relaxed);
            while !curr.is_null() {
                let next = unsafe { (*curr).next.load(Ordering::Relaxed) };
                unsafe {
                    drop(Box::from_raw(curr));
                }
                curr = next;
            }
        }
        drop(buckets);
    }
}
