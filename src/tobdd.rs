use std::cell::Cell;
use std::sync::atomic::Ordering;

#[cfg(feature = "op_stat")]
use std::sync::atomic::{AtomicU64, AtomicUsize};

use ahash::AHashMap;

#[cfg(feature = "op_stat")]
use cpu_time::ThreadTime;

use crate::BddIO;
use crate::spin::distributed_rw::{DistributedRwLock, ReadGuard};
use crate::{
    Bdd, BddManager, BddOp, IoRead, IoWrite, PrintSet,
    cache::{Cache, LockFreeCache},
    hash,
    node::{Node, NodePtr},
    set::{LockFreeSet, Set},
};

#[cfg(feature = "op_stat")]
#[derive(Default)]
pub struct OpStat {
    pub not_cnt: AtomicUsize,

    pub and_cnt: AtomicUsize,

    pub or_cnt: AtomicUsize,

    pub comp_cnt: AtomicUsize,

    pub quant_exist_cnt: AtomicUsize,

    pub quant_forall_cnt: AtomicUsize,

    pub gc_cnt: AtomicUsize,
    pub gc_time: AtomicU64,
    pub gc_freed: AtomicUsize,

    pub grow_cnt: AtomicUsize,
    pub grow_time: AtomicU64,
    pub grow_newed: AtomicUsize,
}

#[cfg(feature = "op_stat")]
impl std::fmt::Display for OpStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "NOT: (cnt: {})\n",
            self.not_cnt.load(Ordering::Relaxed),
        ))?;
        f.write_fmt(format_args!(
            "AND: (cnt: {})\n",
            self.and_cnt.load(Ordering::Relaxed),
        ))?;
        f.write_fmt(format_args!(
            "OR: (cnt: {})\n",
            self.or_cnt.load(Ordering::Relaxed),
        ))?;
        f.write_fmt(format_args!(
            "COMP: (cnt: {})\n",
            self.comp_cnt.load(Ordering::Relaxed),
        ))?;
        f.write_fmt(format_args!(
            "GC: (cnt: {}, time: {} us, freed: {})\n",
            self.gc_cnt.load(Ordering::Relaxed),
            self.gc_time.load(Ordering::Relaxed),
            self.gc_freed.load(Ordering::Relaxed)
        ))?;
        f.write_fmt(format_args!(
            "GROW: (cnt: {}, time: {} us, newed: {})",
            self.grow_cnt.load(Ordering::Relaxed),
            self.grow_time.load(Ordering::Relaxed),
            self.grow_newed.load(Ordering::Relaxed)
        ))?;
        Ok(())
    }
}

#[allow(unused)]
pub struct Manager {
    set: LockFreeSet,
    rw_lock: DistributedRwLock,

    num_vars: usize,

    true_id: Bdd,
    false_id: Bdd,
    vars: Vec<Bdd>,
    nvars: Vec<Bdd>,

    not_cache: LockFreeCache<Bdd, Bdd>,
    and_cache: LockFreeCache<(Bdd, Bdd), Bdd>,
    or_cache: LockFreeCache<(Bdd, Bdd), Bdd>,
    comp_cache: LockFreeCache<(Bdd, Bdd), Bdd>,
    quant_exist_cache: LockFreeCache<(Bdd, Bdd), Bdd>,
    quant_forall_cache: LockFreeCache<(Bdd, Bdd), Bdd>,

    #[cfg(feature = "op_stat")]
    op_stat: OpStat,
    #[cfg(feature = "op_stat")]
    timer: ThreadTime,
}

impl BddManager for Manager {
    fn init(table_size: usize, cache_size: usize, var_num: usize) -> Self {
        let set = LockFreeSet::with_capacity(table_size);

        let true_id = Box::into_raw(Box::new(Node::new(1)));
        set.get_or_insert(true_id);
        true_id.ref_cnt().fetch_add(1, Ordering::Relaxed);
        let false_id = Box::into_raw(Box::new(Node::new(0)));
        set.get_or_insert(false_id);
        false_id.ref_cnt().fetch_add(1, Ordering::Relaxed);
        let mut vars = Vec::with_capacity(var_num);
        let mut nvars = Vec::with_capacity(var_num);
        for i in 0..var_num {
            let var_id = Box::into_raw(Box::new(Node::from(i, false_id, true_id)));
            let nvar_id = Box::into_raw(Box::new(Node::from(i, true_id, false_id)));
            set.get_or_insert(var_id);
            set.get_or_insert(nvar_id);
            var_id.ref_cnt().fetch_add(1, Ordering::Relaxed);
            nvar_id.ref_cnt().fetch_add(1, Ordering::Relaxed);
            vars.push(var_id);
            nvars.push(nvar_id);
        }
        Manager {
            set,
            rw_lock: DistributedRwLock::default(),
            true_id,
            false_id,
            num_vars: var_num,
            vars,
            nvars,
            not_cache: LockFreeCache::with_capacity(cache_size),
            and_cache: LockFreeCache::with_capacity(cache_size),
            or_cache: LockFreeCache::with_capacity(cache_size),
            comp_cache: LockFreeCache::with_capacity(cache_size),
            quant_exist_cache: LockFreeCache::with_capacity(cache_size),
            quant_forall_cache: LockFreeCache::with_capacity(cache_size),
            #[cfg(feature = "op_stat")]
            op_stat: OpStat::default(),
            #[cfg(feature = "op_stat")]
            timer: ThreadTime::now(),
        }
    }

    fn get_var(&self, var: usize) -> Bdd {
        self.vars[var]
    }

    fn get_nvar(&self, var: usize) -> Bdd {
        self.nvars[var]
    }

    fn get_true(&self) -> Bdd {
        self.true_id
    }

    fn get_false(&self) -> Bdd {
        self.false_id
    }

    fn get_node_num(&self) -> usize {
        self.set.entry_num()
    }

    fn deref_bdd(&self, bdd: Bdd) {
        bdd.ref_cnt().fetch_sub(1, Ordering::Relaxed);
    }

    fn gc(&self) -> usize {
        if let Some(_w_guard) = self.rw_lock.try_write() {
            #[cfg(feature = "op_stat")]
            {
                self.op_stat.gc_cnt.fetch_add(1, Ordering::Relaxed);
                self.op_stat
                    .gc_time
                    .fetch_sub(self.timer.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            #[cfg(feature = "op_stat")]
            let before = self.set.entry_num();
            let marked = self.set.mark_nodes();
            self.set.gc_unmarked();
            self.set.unmark_nodes();
            self.and_cache.invalidate_all();
            self.or_cache.invalidate_all();
            self.comp_cache.invalidate_all();
            self.not_cache.invalidate_all();
            self.quant_exist_cache.invalidate_all();
            self.quant_forall_cache.invalidate_all();
            #[cfg(feature = "op_stat")]
            {
                let freed = before.saturating_sub(marked);
                self.op_stat.gc_freed.fetch_add(freed, Ordering::Relaxed);
                self.op_stat
                    .gc_time
                    .fetch_add(self.timer.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            self.set.clear_needs_gc();
            marked
        } else {
            0
        }
    }
}

#[allow(unused)]
impl BddOp for Manager {
    fn not(&self, bdd: Bdd) -> Bdd {
        let _g = self.enter_op();
        #[cfg(feature = "op_stat")]
        {
            self.op_stat.not_cnt.fetch_add(1, Ordering::Relaxed);
        }
        let ret = self._not_rec(bdd);
        self.ref_bdd(ret);
        self.exit_op();
        ret
    }

    fn and(&self, lhs: Bdd, rhs: Bdd) -> Bdd {
        let _g = self.enter_op();
        #[cfg(feature = "op_stat")]
        {
            self.op_stat.and_cnt.fetch_add(1, Ordering::Relaxed);
        }
        let ret = self._and_rec(lhs, rhs);
        self.ref_bdd(ret);
        self.exit_op();
        ret
    }

    fn or(&self, lhs: Bdd, rhs: Bdd) -> Bdd {
        let _g = self.enter_op();
        #[cfg(feature = "op_stat")]
        {
            self.op_stat.or_cnt.fetch_add(1, Ordering::Relaxed);
        }
        let ret = self._or_rec(lhs, rhs);
        self.ref_bdd(ret);
        self.exit_op();
        ret
    }

    fn comp(&self, lhs: Bdd, rhs: Bdd) -> Bdd {
        let _g = self.enter_op();
        #[cfg(feature = "op_stat")]
        {
            self.op_stat.comp_cnt.fetch_add(1, Ordering::Relaxed);
        }
        let ret = self._comp_rec(lhs, rhs);
        self.ref_bdd(ret);
        self.exit_op();
        ret
    }

    fn exist(&self, bdd: Bdd, cube: Bdd) -> Bdd {
        let _g = self.enter_op();
        self.exit_op();
        todo!()
    }

    fn forall(&self, bdd: Bdd, cube: Bdd) -> Bdd {
        let _g = self.enter_op();
        self.exit_op();
        todo!()
    }
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "TOBDD Debug Information")?;
        f.write_fmt(format_args!("node_num: {:?}\n", self.set.entry_num()))?;
        f.write_fmt(format_args!("bucket_size: {:?}\n", self.set.bucket_size()))?;
        f.write_fmt(format_args!("var_num: {:?}\n", self.num_vars))?;
        #[cfg(feature = "cache_stat")]
        {
            f.write_fmt(format_args!("NOT cache stat: {}\n", self.not_cache.stat))?;
            f.write_fmt(format_args!("AND cache stat: {}\n", self.and_cache.stat))?;
            f.write_fmt(format_args!("OR cache stat: {}\n", self.or_cache.stat))?;
            f.write_fmt(format_args!("COMP cache stat: {}\n", self.comp_cache.stat))?;
            f.write_fmt(format_args!(
                "QUANT_EXIST cache stat: {}\n",
                self.quant_exist_cache.stat
            ))?;
            f.write_fmt(format_args!(
                "QUANT_FORALL cache stat: {}\n",
                self.quant_forall_cache.stat
            ))?;
        }
        #[cfg(feature = "table_stat")]
        {
            let report = self.set.table_stat_report();
            f.write_fmt(format_args!("Table stat: {}\n", report))?;
        }
        #[cfg(feature = "op_stat")]
        {
            f.write_fmt(format_args!("Op stat: {}\n", self.op_stat))?;
            f.write_fmt(format_args!("RwLock stat: {}\n", self.rw_lock.stat))?;
        }
        self.set.sanity_check();
        Ok(())
    }
}

impl<W: IoWrite, R: IoRead> BddIO<W, R> for Manager {
    fn serialize(&self, bdd: Bdd, writer: &mut W) -> std::io::Result<()> {
        fn write_buffer_rec<W: IoWrite>(manager: &Manager, mut bdd: Bdd, writer: &mut W) {
            if bdd == manager.true_id || bdd == manager.false_id {
                return;
            }
            write_buffer_rec(manager, *bdd.low(), writer);
            write_buffer_rec(manager, *bdd.high(), writer);
            writer.write_all(&(bdd as usize).to_be_bytes()).unwrap();
            writer.write_all(&(*bdd.level()).to_be_bytes()).unwrap();
            writer
                .write_all(&(*bdd.low() as usize).to_be_bytes())
                .unwrap();
            writer
                .write_all(&(*bdd.high() as usize).to_be_bytes())
                .unwrap();
        }

        for bdd in [self.false_id, self.true_id] {
            writer.write_all(&(bdd as usize).to_be_bytes())?;
        }
        write_buffer_rec(self, bdd, writer);
        writer.flush()?;
        Ok(())
    }

    fn deserialize(&self, reader: &mut R) -> std::io::Result<Bdd> {
        let _r_guard = self.rw_lock.read();
        let mut map: AHashMap<usize, Bdd> = AHashMap::default();
        #[allow(unused_assignments)]
        let (mut bdd, mut level, mut low, mut high, mut ret) =
            (0usize, 0usize, 0usize, 0usize, std::ptr::null_mut());
        let mut window = [0u8; 32];
        reader.read_exact(&mut window[0..16])?;
        let false_id = usize::from_be_bytes(window[0..8].try_into().unwrap());
        let true_id = usize::from_be_bytes(window[8..16].try_into().unwrap());
        map.insert(false_id, self.false_id);
        map.insert(true_id, self.true_id);
        while let Ok(()) = reader.read_exact(&mut window) {
            bdd = usize::from_be_bytes(window[0..8].try_into().unwrap());
            level = usize::from_be_bytes(window[8..16].try_into().unwrap());
            debug_assert!(level <= self.num_vars);
            low = usize::from_be_bytes(window[16..24].try_into().unwrap());
            high = usize::from_be_bytes(window[24..32].try_into().unwrap());
            let mapped_low = *map.get(&low).unwrap();
            let mapped_high = *map.get(&high).unwrap();
            ret = self.make_node(level, mapped_low, mapped_high);
            self.ref_bdd(ret);
            map.insert(bdd, ret);
        }
        for b in map.values() {
            self.deref_bdd(*b);
        }
        self.ref_bdd(ret);
        Ok(ret)
    }
}

impl<W: IoWrite> PrintSet<W> for Manager {
    fn print(&self, bdd: Bdd, f: &mut W) -> std::io::Result<()> {
        fn fmt_rec<W: IoWrite>(
            manager: &Manager,
            f: &mut W,
            chars: &mut Vec<char>,
            mut bdd: Bdd,
            curr: usize,
        ) -> std::io::Result<()> {
            if curr == manager.num_vars {
                for c in chars.iter().take(manager.num_vars) {
                    f.write_fmt(format_args!("{}", c))?;
                }
                f.write_fmt(format_args!("\n"))?;
                return Ok(());
            }
            let level = bdd.level();
            if *level > curr || bdd == manager.true_id {
                chars[curr] = '*';
                fmt_rec(manager, f, chars, bdd, curr + 1)?;
                return Ok(());
            }
            let low = *bdd.low();
            let high = *bdd.high();
            if low != manager.false_id {
                chars[curr] = '0';
                fmt_rec(manager, f, chars, low, curr + 1)?;
            }
            if high != manager.false_id {
                chars[curr] = '1';
                fmt_rec(manager, f, chars, high, curr + 1)?;
            }
            Ok(())
        }

        if bdd == self.false_id {
            f.write_fmt(format_args!("{}", "FALSE\n"))?;
        } else if bdd == self.true_id {
            f.write_fmt(format_args!("{}", "TRUE\n"))?;
        } else {
            let mut set_chars = vec!['-'; self.num_vars];
            fmt_rec(self, f, &mut set_chars, bdd, 0)?;
        }
        Ok(())
    }
}

thread_local! {
    static FREE_ID: Cell<Bdd> = const {Cell::new(std::ptr::null_mut())};
}

impl Manager {
    pub fn ref_bdd(&self, bdd: Bdd) {
        bdd.ref_cnt().fetch_add(1, Ordering::Relaxed);
    }

    fn make_node(&self, level: usize, low: Bdd, high: Bdd) -> Bdd {
        if low == high {
            return low;
        }

        FREE_ID.with(|cell| {
            let mut ptr = cell.get();
            if ptr.is_null() {
                ptr = Box::into_raw(Box::new(Node::new(0)));
                cell.set(ptr);
            }

            *ptr.level() = level;
            *ptr.low() = low;
            *ptr.high() = high;
            ptr.rehash();

            let (present, inserted) = self.set.get_or_insert(ptr);
            if !inserted {
                // we have not use the FREE_ID
                present
            } else {
                cell.set(Box::into_raw(Box::new(Node::new(0))));
                ptr
            }
        })
    }

    fn _not_rec(&self, mut bdd: Bdd) -> Bdd {
        if bdd == self.true_id {
            return self.false_id;
        }
        if bdd == self.false_id {
            return self.true_id;
        }
        let hash: u64 = hash::splitmix64(bdd as u64);
        let cached = self.not_cache.get(hash, &bdd);
        if !cached.is_null() {
            return cached;
        }
        let f_low = self._not_rec(*bdd.low());
        let f_high = self._not_rec(*bdd.high());
        let res = self.make_node(*bdd.level(), f_low, f_high);
        self.not_cache.insert(hash, bdd, res);
        res
    }

    #[inline]
    fn _and_rec(&self, mut lhs: Bdd, mut rhs: Bdd) -> Bdd {
        // sort lhs, rhs without "if"
        (lhs, rhs) = (
            [lhs, rhs][(lhs >= rhs) as usize],
            [lhs, rhs][(lhs < rhs) as usize],
        );
        if lhs == rhs || rhs == self.true_id {
            return lhs;
        }
        if lhs == self.false_id || rhs == self.false_id {
            return self.false_id;
        }
        if lhs == self.true_id {
            return rhs;
        }
        let hash = hash::splitmix64_2(lhs as u64, rhs as u64);
        let bdd = self.and_cache.get(hash, &(lhs, rhs));
        if !bdd.is_null() {
            return bdd;
        }
        let res = match (*lhs.level()).cmp(rhs.level()) {
            std::cmp::Ordering::Less => {
                let f_low = self._and_rec(*lhs.low(), rhs);
                let f_high = self._and_rec(*lhs.high(), rhs);
                self.make_node(*lhs.level(), f_low, f_high)
            }
            std::cmp::Ordering::Greater => {
                let f_low = self._and_rec(lhs, *rhs.low());
                let f_high = self._and_rec(lhs, *rhs.high());
                self.make_node(*rhs.level(), f_low, f_high)
            }
            std::cmp::Ordering::Equal => {
                let f_low = self._and_rec(*lhs.low(), *rhs.low());
                let f_high = self._and_rec(*lhs.high(), *rhs.high());
                self.make_node(*lhs.level(), f_low, f_high)
            }
        };
        self.and_cache.insert(hash, (lhs, rhs), res);
        res
    }

    #[inline]
    fn _or_rec(&self, mut lhs: Bdd, mut rhs: Bdd) -> Bdd {
        // sort lhs, rhs without "if"
        (lhs, rhs) = (
            [lhs, rhs][(lhs >= rhs) as usize],
            [lhs, rhs][(lhs < rhs) as usize],
        );
        if lhs == rhs || rhs == self.false_id {
            return lhs;
        }
        if lhs == self.true_id || rhs == self.true_id {
            return self.true_id;
        }
        if lhs == self.false_id {
            return rhs;
        }
        let hash = hash::splitmix64_2(lhs as u64, rhs as u64);
        let bdd = self.or_cache.get(hash, &(lhs, rhs));
        if !bdd.is_null() {
            return bdd;
        }
        let res = match (*lhs.level()).cmp(rhs.level()) {
            std::cmp::Ordering::Less => {
                let f_low = self._or_rec(*lhs.low(), rhs);
                let f_high = self._or_rec(*lhs.high(), rhs);
                self.make_node(*lhs.level(), f_low, f_high)
            }
            std::cmp::Ordering::Greater => {
                let f_low = self._or_rec(lhs, *rhs.low());
                let f_high = self._or_rec(lhs, *rhs.high());
                self.make_node(*rhs.level(), f_low, f_high)
            }
            std::cmp::Ordering::Equal => {
                let f_low = self._or_rec(*lhs.low(), *rhs.low());
                let f_high = self._or_rec(*lhs.high(), *rhs.high());
                self.make_node(*lhs.level(), f_low, f_high)
            }
        };
        self.or_cache.insert(hash, (lhs, rhs), res);
        res
    }

    #[inline]
    fn _comp_rec(&self, mut lhs: Bdd, mut rhs: Bdd) -> Bdd {
        if lhs == rhs || lhs == self.false_id || rhs == self.true_id {
            return self.false_id;
        }
        if rhs == self.false_id {
            return lhs;
        }
        if lhs == self.true_id {
            return self._not_rec(rhs);
        }
        let hash = hash::splitmix64_2(lhs as u64, rhs as u64);
        let bdd = self.comp_cache.get(hash, &(lhs, rhs));
        if !bdd.is_null() {
            return bdd;
        }
        let res = match (*lhs.level()).cmp(rhs.level()) {
            std::cmp::Ordering::Less => {
                let f_low = self._comp_rec(*lhs.low(), rhs);
                let f_high = self._comp_rec(*lhs.high(), rhs);
                self.make_node(*lhs.level(), f_low, f_high)
            }
            std::cmp::Ordering::Greater => {
                let f_low = self._comp_rec(lhs, *rhs.low());
                let f_high = self._comp_rec(lhs, *rhs.high());
                self.make_node(*rhs.level(), f_low, f_high)
            }
            std::cmp::Ordering::Equal => {
                let f_low = self._comp_rec(*lhs.low(), *rhs.low());
                let f_high = self._comp_rec(*lhs.high(), *rhs.high());
                self.make_node(*lhs.level(), f_low, f_high)
            }
        };
        self.comp_cache.insert(hash, (lhs, rhs), res);
        res
    }

    const MAX_LOAD_FACTOR: usize = 1usize;
    const MIN_GC_RATIO: f64 = 0.25;

    fn enter_op(&self) -> ReadGuard<'_> {
        let entry_num = self.set.entry_num();
        let bucket_size = self.set.bucket_size();
        if entry_num >= bucket_size * Self::MAX_LOAD_FACTOR
            && let Some(_w_guard) = self.rw_lock.try_write()
        {
                let marked = self.set.mark_nodes();
                if entry_num - marked < (bucket_size as f64 * Self::MIN_GC_RATIO) as usize {
                    #[cfg(feature = "op_stat")]
                    {
                        self.op_stat.grow_cnt.fetch_add(1, Ordering::Relaxed);
                        self.op_stat
                            .grow_time
                            .fetch_sub(self.timer.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                    self.set.unmark_nodes();
                    self.set.grow();
                    self.and_cache.grow();
                    self.or_cache.grow();
                    self.comp_cache.grow();
                    self.not_cache.grow();
                    self.quant_exist_cache.grow();
                    self.quant_forall_cache.grow();
                    #[cfg(feature = "op_stat")]
                    {
                        let new_bucket_size = self.set.bucket_size();
                        let newed = new_bucket_size.saturating_sub(bucket_size);
                        self.op_stat.grow_newed.fetch_add(newed, Ordering::Relaxed);
                        self.op_stat
                            .grow_time
                            .fetch_add(self.timer.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                } else {
                    #[cfg(feature = "op_stat")]
                    {
                        self.op_stat.gc_cnt.fetch_add(1, Ordering::Relaxed);
                        self.op_stat
                            .gc_time
                            .fetch_sub(self.timer.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                    self.set.gc_unmarked();
                    self.set.unmark_nodes();
                    self.and_cache.invalidate_all();
                    self.or_cache.invalidate_all();
                    self.comp_cache.invalidate_all();
                    self.not_cache.invalidate_all();
                    self.quant_exist_cache.invalidate_all();
                    self.quant_forall_cache.invalidate_all();
                    #[cfg(feature = "op_stat")]
                    {
                        let freed = entry_num.saturating_sub(marked);
                        self.op_stat.gc_freed.fetch_add(freed, Ordering::Relaxed);
                        self.op_stat
                            .gc_time
                            .fetch_add(self.timer.elapsed().as_micros() as u64, Ordering::Relaxed);
                    }
                }
        }

        self.rw_lock.read()
    }

    fn exit_op(&self) {}
}

#[cfg(test)]
mod tests {
    use std::str::from_utf8;

    use flate2::Compression;

    use crate::BddIO;

    use super::*;
    #[test]
    fn test_and() {
        let manager: Manager = Manager::init(1024, 1024, 3);
        let a = manager.get_nvar(0);
        let b = manager.get_nvar(1);
        let c = manager.get_nvar(2);

        let mut buf = Vec::new();

        let and_ab = manager.and(a, b);
        let and_abc = manager.and(and_ab, c);

        manager.print(and_abc, &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "000\n");

        manager.deref_bdd(and_ab);
        manager.deref_bdd(and_abc);
    }

    #[test]
    fn test_comp() {
        let manager: Manager = Manager::init(1024, 1024, 3);
        let a = manager.get_var(0);
        let nb = manager.get_nvar(1);
        let _c = manager.get_var(2);

        let mut buf = Vec::new();

        let a_and_nb = manager.and(a, nb);
        let comp = manager.comp(a, a_and_nb);

        manager.print(comp, &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "11*\n");

        manager.deref_bdd(a_and_nb);
        manager.deref_bdd(comp);
    }

    #[test]
    fn test_gc() {
        let manager = Manager::init(8, 8, 3);
        let a = manager.get_var(0);
        let b = manager.get_var(1);
        let c = manager.get_var(2);

        let ab = manager.and(a, b);
        let bc = manager.and(b, c);
        manager.gc();

        assert_eq!(manager.get_node_num(), 10);

        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.gc();
        assert_eq!(manager.get_node_num(), 8);
    }

    #[test]
    fn test_print_set() {
        let manager: Manager = Manager::init(1024, 1024, 3);
        let a = manager.get_var(0);
        let b = manager.get_var(1);
        let c = manager.get_var(2);

        let ab = manager.and(a, b);
        let bc = manager.and(b, c);
        let abc = manager.or(ab, bc);

        let mut buf = Vec::new();

        PrintSet::print(&manager, manager.get_true(), &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "TRUE\n");
        buf.clear();
        PrintSet::print(&manager, manager.get_false(), &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "FALSE\n");
        buf.clear();
        PrintSet::print(&manager, abc, &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "011\n11*\n");

        buf.clear();
        PrintSet::print(&manager, ab, &mut buf).unwrap();
        dbg!(from_utf8(&buf).unwrap());

        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.deref_bdd(abc);
    }

    #[test]
    fn test_ruddy_io() {
        let manager: Manager = Manager::init(1024, 1024, 3);
        let a = manager.get_var(0);
        let b = manager.get_var(1);
        let c = manager.get_var(2);

        let ab = manager.and(a, b);
        let bc = manager.and(b, c);
        let abc = manager.and(ab, bc);

        let mut buffer = Vec::new();
        BddIO::<Vec<u8>, &[u8]>::serialize(&manager, abc, &mut buffer).unwrap();
        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.deref_bdd(abc);

        let another_manager: Manager = Manager::init(1024, 1024, 3);
        let a = another_manager.get_var(0);
        let b = another_manager.get_var(1);
        let c = another_manager.get_var(2);

        let ab = another_manager.and(a, b);
        let bc = another_manager.and(b, c);
        let abc = another_manager.and(ab, bc);
        let another_abc =
            BddIO::<Vec<u8>, &[u8]>::deserialize(&another_manager, &mut &buffer[..]).unwrap();

        assert_eq!(abc, another_abc);

        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.deref_bdd(abc);
        another_manager.deref_bdd(another_abc);
    }

    #[test]
    fn test_ruddy_io_compressed() {
        const VAR_NUM: usize = 32;

        let manager: Manager = Manager::init(1024, 1024, VAR_NUM);
        let mut and_all = manager.get_true();
        let mut tmp: Bdd;
        for i in 0..VAR_NUM {
            let var = manager.get_var(i);
            tmp = manager.and(and_all, var);
            manager.deref_bdd(and_all);
            and_all = tmp;
        }

        let mut buffer_uncomp = Vec::new();
        BddIO::<Vec<u8>, &[u8]>::serialize(&manager, and_all, &mut buffer_uncomp).unwrap();
        println!("Uncompressed size: {}", buffer_uncomp.len());

        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), Compression::fast());
        BddIO::<flate2::write::DeflateEncoder<Vec<u8>>, flate2::read::DeflateDecoder<&[u8]>>::serialize(&manager, and_all, &mut encoder).unwrap();
        let buffer = encoder.finish().unwrap();
        manager.deref_bdd(and_all);
        println!("Compressed size: {}", buffer.len());
        println!(
            "Compression ratio: {}",
            1f64 - (buffer.len() as f64 / buffer_uncomp.len() as f64)
        );

        let another_manager: Manager = Manager::init(1024, 1024, VAR_NUM);
        let mut decoder = flate2::read::DeflateDecoder::new(&buffer[..]);
        let another_and_all = BddIO::<
            flate2::write::DeflateEncoder<Vec<u8>>,
            flate2::read::DeflateDecoder<&[u8]>,
        >::deserialize(&another_manager, &mut decoder)
        .unwrap();

        // print to stdout to make sure the BDD is correct
        // let mut stdout = std::io::stdout();
        // another_manager.print(another_and_all, &mut stdout).unwrap();

        another_manager.deref_bdd(another_and_all);
    }
}
