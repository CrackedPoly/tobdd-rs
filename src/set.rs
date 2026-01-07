use std::cell::Cell;
use std::cmp::Ordering as CmpOrd;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::node::{Node, NodePtr};

#[allow(dead_code)]
pub trait Set {
    // concurrent operations
    fn get_or_insert(&self, new_ptr: *mut Node) -> (*mut Node, bool);

    // single-threaded operations
    fn mark_nodes(&self) -> usize;
    fn gc_unmarked(&self);
    fn unmark_nodes(&self);
    fn grow(&self);
}

pub struct LockFreeSet {
    buckets: Cell<*mut [AtomicPtr<Node>]>,
    size_exp: Cell<usize>,
    num_marks: Cell<usize>,
    num_entry: AtomicUsize,
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
        }
    }

    pub fn bucket_size(&self) -> usize {
        1 << self.size_exp.get()
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
}

impl Set for LockFreeSet {
    // 链表只增长、不删除节点（因此读到的指针永远有效，不会被释放）；
    //节点插入后，level/low/high 不再改变；
    // ptr 指向的节点是有效的、且当前不在链表中；
    // 用 (level, low, high) 作为“值相等”的判定，并按这个 key 升序插入（保证并发下不会插入重复值：第二个线程 CAS 失败后会重试并看到已存在节点）。
    fn get_or_insert(&self, new_ptr: *mut Node) -> (*mut Node, bool) {
        let idx = new_ptr.node_hash() & ((1 << self.size_exp.get()) - 1);
        let head = &(unsafe { &*self.buckets.get() })[idx as usize];

        // 只读一次新节点的 key（要求调用者不会并发修改这些字段）
        let new_key = unsafe { &*new_ptr }.key();

        'retry: loop {
            // prev 指向“指向当前节点的那个 AtomicPtr”（可能是 head，也可能是某个节点的 next）
            let mut prev: &AtomicPtr<Node> = head;
            let mut curr: *mut Node = prev.load(Ordering::Acquire);

            while !curr.is_null() {
                // 由于没有删除，curr 指向的 Node 永远有效
                let curr_ref = unsafe { &*curr };
                let curr_key = curr_ref.key();

                match curr_key.cmp(&new_key) {
                    CmpOrd::Equal => {
                        // 找到相同值：不插入，返回已存在节点
                        return (curr, false);
                    }
                    CmpOrd::Greater => {
                        // 应该插入在 curr 之前（也就是 prev -> curr 之间）
                        break;
                    }
                    CmpOrd::Less => {
                        // 继续向后走
                        prev = &curr_ref.next;
                        curr = prev.load(Ordering::Acquire) as _;
                    }
                }
            }

            // 走到这里表示：没看到相同 key，且插入位置是 prev -> curr 之间
            // 先把新节点的 next 指向 curr（此时新节点还没发布到链表里）
            unsafe {
                (*new_ptr).next.store(curr, Ordering::Relaxed);
            }

            // CAS：如果 prev 仍然指向 curr，则把 prev 改为指向 ptr_new
            match prev.compare_exchange(
                curr,
                new_ptr,
                Ordering::AcqRel, // 成功：发布新节点（Release），并同步读到的链表状态（Acquire）
                Ordering::Acquire, // 失败：获取最新的 prev 值
            ) {
                Ok(_) => {
                    // 插入成功
                    self.num_entry.fetch_add(1, Ordering::Relaxed);
                    return (new_ptr, true);
                }
                Err(_actual_now) => {
                    // 并发有变化（有人插入了节点），重试：
                    // 关键点：重试会重新遍历并检查 Equal，从而避免并发下插入重复 key。
                    continue 'retry;
                }
            }
        }
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
        assert!(counter <= (2 << self.size_exp.get()));
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
