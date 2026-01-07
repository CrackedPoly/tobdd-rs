use std::{
    cell::Cell,
    hash::BuildHasherDefault,
    ops::DerefMut,
    sync::atomic::{AtomicUsize, Ordering},
};

use ahash::{AHashMap, AHasher};

use crate::{
    BddIO, BddManager, BddOp, IoRead, IoWrite, PrintSet,
    alloc::Allocator,
    cache::{Cache, LockFreeCache},
    hash,
    node::{Idx, Node, NodePtr},
    set::{LockFreeSet, Set},
};

#[allow(unused)]
pub struct Manager<I: Idx, A: Allocator<I>> {
    alloc: Box<A>,
    set: LockFreeSet<I, A>,

    num_vars: usize,
    num_nodes: AtomicUsize,

    true_id: I,
    false_id: I,
    vars: Vec<I>,
    nvars: Vec<I>,

    not_cache: LockFreeCache<I, I>,
    and_cache: LockFreeCache<(I, I), I>,
    or_cache: LockFreeCache<(I, I), I>,
    comp_cache: LockFreeCache<(I, I), I>,
    quant_exist_cache: LockFreeCache<(I, I), I>,
    quant_forall_cache: LockFreeCache<(I, I), I>,
}

impl<A: Allocator<usize>> BddManager<usize> for Manager<usize, A> {
    fn init(table_size: usize, cache_size: usize, var_num: usize) -> Self {
        let alloc = Box::new(A::default());
        let set = LockFreeSet::with_capacity(table_size);

        let true_id = alloc.alloc(Node::from(0, usize::MAX - 1, usize::MAX));
        set.get_or_insert(
            hash::splitmix64_3(0, (usize::MAX - 1) as u64, usize::MAX as u64),
            NodePtr::from(true_id, *alloc),
        );
        alloc.index(true_id).ref_cnt.fetch_add(1, Ordering::Relaxed);
        let false_id = alloc.alloc(Node::from(0, usize::MAX, usize::MAX - 1));
        set.get_or_insert(
            hash::splitmix64_3(0, usize::MAX as u64, (usize::MAX - 1) as u64),
            NodePtr::from(false_id, *alloc),
        );
        alloc
            .index(false_id)
            .ref_cnt
            .fetch_add(1, Ordering::Relaxed);
        let mut vars = Vec::with_capacity(var_num);
        let mut nvars = Vec::with_capacity(var_num);
        for i in 0..var_num {
            let var_id = alloc.alloc(Node::from(i, false_id, true_id));
            let nvar_id = alloc.alloc(Node::from(i, true_id, false_id));
            set.get_or_insert(
                hash::splitmix64_3(i as u64, false_id as u64, true_id as u64),
                NodePtr::from(var_id, *alloc),
            );
            set.get_or_insert(
                hash::splitmix64_3(i as u64, true_id as u64, false_id as u64),
                NodePtr::from(nvar_id, *alloc),
            );
            alloc.index(var_id).ref_cnt.fetch_add(1, Ordering::Relaxed);
            alloc.index(nvar_id).ref_cnt.fetch_add(1, Ordering::Relaxed);
            vars.push(var_id);
            nvars.push(nvar_id);
        }
        Manager {
            alloc,
            set,
            true_id,
            false_id,
            num_nodes: AtomicUsize::new(2),
            num_vars: var_num,
            vars,
            nvars,
            not_cache: LockFreeCache::with_capacity(cache_size),
            and_cache: LockFreeCache::with_capacity(cache_size),
            or_cache: LockFreeCache::with_capacity(cache_size),
            comp_cache: LockFreeCache::with_capacity(cache_size),
            quant_exist_cache: LockFreeCache::with_capacity(cache_size),
            quant_forall_cache: LockFreeCache::with_capacity(cache_size),
        }
    }

    fn get_var(&self, var: usize) -> usize {
        self.vars[var]
    }

    fn get_nvar(&self, var: usize) -> usize {
        self.nvars[var]
    }

    fn get_true(&self) -> usize {
        self.true_id
    }

    fn get_false(&self) -> usize {
        self.false_id
    }

    fn get_node_num(&self) -> usize {
        self.num_nodes.load(Ordering::Relaxed)
    }

    fn ref_bdd(&self, bdd: usize) {
        self.alloc
            .index(bdd)
            .ref_cnt
            .fetch_add(1, Ordering::Relaxed);
    }

    fn deref_bdd(&self, bdd: usize) {
        self.alloc
            .index(bdd)
            .ref_cnt
            .fetch_sub(1, Ordering::Relaxed);
    }

    fn gc(&self) -> usize {
        todo!()
    }
}

#[allow(unused)]
impl<A: Allocator<usize>> BddOp<usize> for Manager<usize, A> {
    fn not(&self, bdd: usize) -> usize {
        self._not_rec(bdd)
    }

    fn and(&self, lhs: usize, rhs: usize) -> usize {
        self._and_rec(lhs, rhs)
    }

    fn or(&self, lhs: usize, rhs: usize) -> usize {
        self._or_rec(lhs, rhs)
    }

    fn comp(&self, lhs: usize, rhs: usize) -> usize {
        self._comp_rec(lhs, rhs)
    }

    fn exist(&self, bdd: usize, cube: usize) -> usize {
        todo!()
    }

    fn forall(&self, bdd: usize, cube: usize) -> usize {
        todo!()
    }
}

impl<A: Allocator<usize>, W: IoWrite, R: IoRead> BddIO<usize, W, R> for Manager<usize, A> {
    fn serialize(&self, bdd: usize, writer: &mut W) -> std::io::Result<()> {
        fn write_buffer_rec<W: IoWrite, A: Allocator<usize>>(
            manager: &Manager<usize, A>,
            bdd: usize,
            writer: &mut W,
        ) {
            if bdd == manager.true_id || bdd == manager.false_id {
                return;
            }
            write_buffer_rec(manager, manager.low(bdd), writer);
            write_buffer_rec(manager, manager.high(bdd), writer);
            writer.write_all(&bdd.to_be_bytes()).unwrap();
            writer.write_all(&manager.level(bdd).to_be_bytes()).unwrap();
            writer.write_all(&manager.low(bdd).to_be_bytes()).unwrap();
            writer.write_all(&manager.high(bdd).to_be_bytes()).unwrap();
        }

        for bdd in [self.false_id, self.true_id] {
            writer.write_all(&bdd.to_be_bytes())?;
        }
        write_buffer_rec(self, bdd, writer);
        writer.flush()?;
        Ok(())
    }

    fn deserialize(&self, reader: &mut R) -> std::io::Result<usize> {
        let mut map: AHashMap<usize, usize> = AHashMap::default();
        #[allow(unused_assignments)]
        let (mut bdd, mut level, mut low, mut high, mut ret) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
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
        Ok(ret)
    }
}

impl<A: Allocator<usize>, W: IoWrite> PrintSet<usize, W> for Manager<usize, A> {
    fn print(&self, bdd: usize, f: &mut W) -> std::io::Result<()> {
        fn fmt_rec<W: IoWrite, A: Allocator<usize>>(
            manager: &Manager<usize, A>,
            f: &mut W,
            chars: &mut Vec<char>,
            bdd: usize,
            curr: usize,
        ) -> std::io::Result<()> {
            if curr == manager.num_vars {
                for c in chars.iter().take(manager.num_vars) {
                    f.write_fmt(format_args!("{}", c))?;
                }
                f.write_fmt(format_args!("\n"))?;
                return Ok(());
            }
            let level = manager.level(bdd);
            if level > curr || bdd == manager.true_id {
                chars[curr] = '*';
                fmt_rec(manager, f, chars, bdd, curr + 1)?;
                return Ok(());
            }
            let low = manager.low(bdd);
            let high = manager.high(bdd);
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
    static FREE_ID: Cell<usize> = const {Cell::new(usize::NULL)};
}

impl<A: Allocator<usize>> Manager<usize, A> {
    #[inline]
    fn index_mut(&self, bdd: usize) -> impl DerefMut<Target = Node<usize, A>> {
        self.alloc.index_mut(bdd)
    }

    #[inline]
    fn level(&self, bdd: usize) -> usize {
        self.alloc.index(bdd).level
    }

    #[inline]
    fn low(&self, bdd: usize) -> usize {
        self.alloc.index(bdd).low
    }

    #[inline]
    fn high(&self, bdd: usize) -> usize {
        self.alloc.index(bdd).high
    }

    fn make_node(&self, level: usize, low: usize, high: usize) -> usize {
        if low == high {
            return low;
        }

        FREE_ID.with(|cell| {
            let mut idx = cell.get();
            if idx == usize::NULL {
                idx = self.alloc.alloc(Node::from(0, 0, 0));
                cell.set(idx);
            }

            self.index_mut(idx).level = level;
            self.index_mut(idx).low = low;
            self.index_mut(idx).high = high;

            let nref = NodePtr::from(idx, *self.alloc);

            let (present, inserted) = self.set.get_or_insert(
                hash::splitmix64_3(level as u64, low as u64, high as u64),
                nref,
            );
            if !inserted {
                // we have not use the FREE_ID
                present
            } else {
                self.num_nodes.fetch_add(1, Ordering::Relaxed);
                cell.set(self.alloc.alloc(Node::from(0, 0, 0)));
                idx
            }
        })
    }

    fn _not_rec(&self, bdd: usize) -> usize {
        if bdd == self.true_id {
            return self.false_id;
        }
        if bdd == self.false_id {
            return self.true_id;
        }
        let hash: u64 = hash::splitmix64(bdd as u64);
        let cached = self.not_cache.get(hash, &bdd);
        if cached != usize::NULL {
            return cached;
        }
        let f_low = self._not_rec(self.low(bdd));
        let f_high = self._not_rec(self.high(bdd));
        let res = self.make_node(self.level(bdd), f_low, f_high);
        self.not_cache.insert(hash, bdd, res);
        res
    }

    #[inline]
    fn _and_rec(&self, mut lhs: usize, mut rhs: usize) -> usize {
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
        if bdd != usize::NULL {
            return bdd;
        }
        let res = match self.level(lhs).cmp(&self.level(rhs)) {
            std::cmp::Ordering::Less => {
                let f_low = self._and_rec(self.low(lhs), rhs);
                let f_high = self._and_rec(self.high(lhs), rhs);
                self.make_node(self.level(lhs), f_low, f_high)
            }
            std::cmp::Ordering::Greater => {
                let f_low = self._and_rec(lhs, self.low(rhs));
                let f_high = self._and_rec(lhs, self.high(rhs));
                self.make_node(self.level(rhs), f_low, f_high)
            }
            std::cmp::Ordering::Equal => {
                let f_low = self._and_rec(self.low(lhs), self.low(rhs));
                let f_high = self._and_rec(self.high(lhs), self.high(rhs));
                self.make_node(self.level(lhs), f_low, f_high)
            }
        };
        self.and_cache.insert(hash, (lhs, rhs), res);
        res
    }

    #[inline]
    fn _or_rec(&self, mut lhs: usize, mut rhs: usize) -> usize {
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
        if bdd != usize::NULL {
            return bdd;
        }
        let res = match self.level(lhs).cmp(&self.level(rhs)) {
            std::cmp::Ordering::Less => {
                let f_low = self._or_rec(self.low(lhs), rhs);
                let f_high = self._or_rec(self.high(lhs), rhs);
                self.make_node(self.level(lhs), f_low, f_high)
            }
            std::cmp::Ordering::Greater => {
                let f_low = self._or_rec(lhs, self.low(rhs));
                let f_high = self._or_rec(lhs, self.high(rhs));
                self.make_node(self.level(rhs), f_low, f_high)
            }
            std::cmp::Ordering::Equal => {
                let f_low = self._or_rec(self.low(lhs), self.low(rhs));
                let f_high = self._or_rec(self.high(lhs), self.high(rhs));
                self.make_node(self.level(lhs), f_low, f_high)
            }
        };
        self.or_cache.insert(hash, (lhs, rhs), res);
        res
    }

    #[inline]
    fn _comp_rec(&self, lhs: usize, rhs: usize) -> usize {
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
        if bdd != usize::NULL {
            return bdd;
        }
        let res = match self.level(lhs).cmp(&self.level(rhs)) {
            std::cmp::Ordering::Less => {
                let f_low = self._comp_rec(self.low(lhs), rhs);
                let f_high = self._comp_rec(self.high(lhs), rhs);
                self.make_node(self.level(lhs), f_low, f_high)
            }
            std::cmp::Ordering::Greater => {
                let f_low = self._comp_rec(lhs, self.low(rhs));
                let f_high = self._comp_rec(lhs, self.high(rhs));
                self.make_node(self.level(rhs), f_low, f_high)
            }
            std::cmp::Ordering::Equal => {
                let f_low = self._comp_rec(self.low(lhs), self.low(rhs));
                let f_high = self._comp_rec(self.high(lhs), self.high(rhs));
                self.make_node(self.level(lhs), f_low, f_high)
            }
        };
        self.comp_cache.insert(hash, (lhs, rhs), res);
        res
    }
}

#[cfg(test)]
mod tests {
    use std::str::from_utf8;

    use flate2::Compression;

    use super::*;
    #[test]
    fn test_and() {
        let manager: Manager<usize, ()> = Manager::init(1024, 1024, 3);
        let a = manager.get_nvar(0);
        let b = manager.get_nvar(1);
        let c = manager.get_nvar(2);

        let mut buf = Vec::new();

        let and_ab = manager.and(a, b);
        manager.ref_bdd(and_ab);
        let and_abc = manager.and(and_ab, c);
        manager.ref_bdd(and_abc);

        manager.print(and_abc, &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "000\n");

        manager.deref_bdd(and_ab);
        manager.deref_bdd(and_abc);
    }

    #[test]
    fn test_comp() {
        let manager: Manager<usize, ()> = Manager::init(1024, 1024, 3);
        let a = manager.get_var(0);
        let nb = manager.get_nvar(1);
        let _c = manager.get_var(2);

        let mut buf = Vec::new();

        let a_and_nb = manager.and(a, nb);
        manager.ref_bdd(a_and_nb);
        let comp = manager.comp(a, a_and_nb);
        manager.ref_bdd(comp);

        manager.print(comp, &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "11*\n");

        manager.deref_bdd(a_and_nb);
        manager.deref_bdd(comp);
    }

    #[test]
    fn test_print_set() {
        let manager: Manager<usize, ()> = Manager::init(1024, 1024, 3);
        let a = manager.get_var(0);
        let b = manager.get_var(1);
        let c = manager.get_var(2);

        let ab = manager.and(a, b);
        manager.ref_bdd(ab);
        let bc = manager.and(b, c);
        manager.ref_bdd(bc);
        let abc = manager.or(ab, bc);
        manager.ref_bdd(abc);

        let mut buf = Vec::new();

        PrintSet::print(&manager, manager.get_true(), &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "TRUE\n");
        buf.clear();
        PrintSet::print(&manager, manager.get_false(), &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "FALSE\n");
        buf.clear();
        PrintSet::print(&manager, abc, &mut buf).unwrap();
        debug_assert_eq!(from_utf8(&buf).unwrap(), "011\n11*\n");

        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.deref_bdd(abc);
    }

    #[test]
    fn test_ruddy_io() {
        let manager: Manager<usize, ()> = Manager::init(1024, 1024, 3);
        let a = manager.get_var(0);
        let b = manager.get_var(1);
        let c = manager.get_var(2);

        let ab = manager.and(a, b);
        manager.ref_bdd(ab);
        let bc = manager.and(b, c);
        manager.ref_bdd(bc);
        let abc = manager.and(ab, bc);
        manager.ref_bdd(abc);

        let mut buffer = Vec::new();
        BddIO::<usize, Vec<u8>, &[u8]>::serialize(&manager, abc, &mut buffer).unwrap();
        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.deref_bdd(abc);

        let another_manager: Manager<usize, ()> = Manager::init(1024, 1024, 3);
        let a = another_manager.get_var(0);
        let b = another_manager.get_var(1);
        let c = another_manager.get_var(2);

        let ab = another_manager.and(a, b);
        another_manager.ref_bdd(ab);
        let bc = another_manager.and(b, c);
        another_manager.ref_bdd(bc);
        let abc = another_manager.and(ab, bc);
        another_manager.ref_bdd(abc);
        let another_abc =
            BddIO::<usize, Vec<u8>, &[u8]>::deserialize(&another_manager, &mut &buffer[..])
                .unwrap();
        another_manager.ref_bdd(another_abc);

        assert_eq!(abc, another_abc);

        manager.deref_bdd(ab);
        manager.deref_bdd(bc);
        manager.deref_bdd(abc);
        another_manager.deref_bdd(another_abc);
    }

    #[test]
    fn test_ruddy_io_compressed() {
        const VAR_NUM: usize = 32;

        let manager: Manager<usize, ()> = Manager::init(1024, 1024, VAR_NUM);
        let mut and_all = manager.get_true();
        let mut tmp: usize;
        for i in 0..VAR_NUM {
            let var = manager.get_var(i);
            tmp = manager.and(and_all, var);
            manager.ref_bdd(tmp);
            manager.deref_bdd(and_all);
            and_all = tmp;
        }

        let mut buffer_uncomp = Vec::new();
        BddIO::<usize, Vec<u8>, &[u8]>::serialize(&manager, and_all, &mut buffer_uncomp).unwrap();
        println!("Uncompressed size: {}", buffer_uncomp.len());

        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), Compression::fast());
        BddIO::<usize, flate2::write::DeflateEncoder<Vec<u8>>, flate2::read::DeflateDecoder<&[u8]>>::serialize(&manager, and_all, &mut encoder).unwrap();
        let buffer = encoder.finish().unwrap();
        manager.deref_bdd(and_all);
        println!("Compressed size: {}", buffer.len());
        println!(
            "Compression ratio: {}",
            1f64 - (buffer.len() as f64 / buffer_uncomp.len() as f64)
        );

        let another_manager: Manager<usize, ()> = Manager::init(1024, 1024, VAR_NUM);
        let mut decoder = flate2::read::DeflateDecoder::new(&buffer[..]);
        let another_and_all = BddIO::<
            usize,
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
