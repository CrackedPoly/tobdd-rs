use std::{borrow::Borrow, cell::Cell, hash::Hash, sync::atomic::Ordering};

use crate::spin::rw_r_pre::SpinRwLock;

#[cfg(feature = "cache_stat")]
use std::fmt::Display;
#[cfg(feature = "cache_stat")]
use std::sync::atomic::AtomicUsize;

pub trait Cache<K, V> {
    fn get<Q>(&self, hash: u64, q: &Q) -> V
    where
        Q: Borrow<K> + Hash;

    fn insert(&self, hash: u64, key: K, value: V) -> bool;

    fn invalidate_all(&self);

    fn grow(&self);
}

pub struct LockFreeCache<K, V> {
    entries: Cell<*mut [SpinRwLock<(K, V)>]>,
    size_exp: Cell<usize>,
    #[cfg(feature = "cache_stat")]
    pub stat: CacheStat,
}

impl<K: Default, V: Default> LockFreeCache<K, V> {
    pub fn with_capacity(cap: usize) -> Self {
        let size_exp = cap.isolate_highest_one().trailing_zeros() + 1;
        let entries = (0..(1 << size_exp))
            .map(|_| SpinRwLock::new(Default::default()))
            .collect::<Vec<SpinRwLock<(K, V)>>>()
            .into_boxed_slice();
        LockFreeCache {
            entries: Cell::new(Box::into_raw(entries)),
            size_exp: Cell::new(size_exp as usize),
            #[cfg(feature = "cache_stat")]
            stat: CacheStat::default(),
        }
    }
}

impl<K: Hash + Eq + Default, V: Default + Copy> Cache<K, V> for LockFreeCache<K, V> {
    fn get<Q>(&self, hash: u64, q: &Q) -> V
    where
        Q: Borrow<K> + Hash,
    {
        let idx = hash & ((1 << self.size_exp.get()) - 1);
        let read_guard = unsafe { &*self.entries.get() }[idx as usize].read();
        if read_guard.0 == *q.borrow() {
            #[cfg(feature = "cache_stat")]
            self.stat.record_hit();
            return read_guard.1;
        }

        #[cfg(feature = "cache_stat")]
        self.stat.record_miss();
        V::default()
    }

    fn insert(&self, hash: u64, key: K, value: V) -> bool {
        let idx = hash & ((1 << self.size_exp.get()) - 1);
        let lock = &(unsafe { &*self.entries.get() }[idx as usize]);
        // blocking write - waits for readers to finish
        lock.write(|pair| {
            pair.0 = key;
            pair.1 = value;
        });
        true
    }

    fn invalidate_all(&self) {
        for entry in (unsafe { &*self.entries.get() }).iter() {
            let _ = entry.try_write_once(|pair| {
                pair.0 = Default::default();
                pair.1 = Default::default();
            });
        }
        #[cfg(feature = "cache_stat")]
        self.stat.record_clear();
    }

    fn grow(&self) {
        // double the size
        let new_size_exp = self.size_exp.get() + 1;
        let new_entries = (0..(1 << new_size_exp))
            .map(|_| SpinRwLock::new(Default::default()))
            .collect::<Vec<SpinRwLock<(K, V)>>>()
            .into_boxed_slice();
        drop(unsafe { Box::from_raw(self.entries.get()) });
        self.entries.set(Box::into_raw(new_entries));
        self.size_exp.set(new_size_exp);
        #[cfg(feature = "cache_stat")]
        self.stat.record_grow();
    }
}

#[cfg(feature = "cache_stat")]
#[derive(Default)]
pub struct CacheStat {
    pub clear_cnt: AtomicUsize,
    pub grow_cnt: AtomicUsize,
    pub last_hit: AtomicUsize, // hit count since last grow
    pub last_miss: AtomicUsize,
    pub unique_hit: AtomicUsize, // total hit count
    pub unique_miss: AtomicUsize,
}

#[cfg(feature = "cache_stat")]
impl CacheStat {
    #[inline]
    fn record_hit(&self) {
        self.last_hit.fetch_add(1, Ordering::Relaxed);
        self.unique_hit.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn record_miss(&self) {
        self.last_miss.fetch_add(1, Ordering::Relaxed);
        self.unique_miss.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn record_grow(&self) {
        self.grow_cnt.fetch_add(1, Ordering::Relaxed);
        self.last_hit.store(0, Ordering::Relaxed);
        self.last_miss.store(0, Ordering::Relaxed);
    }

    #[inline]
    fn record_clear(&self) {
        self.clear_cnt.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(feature = "cache_stat")]
impl Display for CacheStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unique_hit = self.unique_hit.load(Ordering::Relaxed);
        let unique_miss = self.unique_miss.load(Ordering::Relaxed);
        write!(
            f,
            "unique_hit: {}, unique_miss: {}",
            unique_hit, unique_miss
        )
    }
}
