use std::{
    borrow::Borrow,
    hash::Hash,
    sync::atomic::{AtomicIsize, Ordering},
};

use crate::node::Idx;

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::ops::Deref;

pub struct SpinRwLock<T> {
    state: AtomicIsize,
    data: UnsafeCell<T>,
}

pub struct ReadGuard<'a, T> {
    lock: &'a SpinRwLock<T>,
}

unsafe impl<T: Send> Send for SpinRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for SpinRwLock<T> {}

impl<T> SpinRwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicIsize::new(0),
            data: UnsafeCell::new(value),
        }
    }

    #[inline]
    pub fn read(&self) -> ReadGuard<'_, T> {
        loop {
            let s = self.state.load(Ordering::Acquire);

            // there is a write, spin
            if s < 0 {
                spin_loop();
                continue;
            }

            if s == isize::MAX {
                panic!("reader count overflow");
            }

            if self
                .state
                .compare_exchange_weak(s, s + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return ReadGuard { lock: self };
            }
        }
    }

    /// Writing is wait-free.
    #[inline]
    pub fn try_write_once<F>(&self, f: F) -> bool
    where
        F: FnOnce(&mut T),
    {
        let s = self.state.load(Ordering::Acquire);

        if s != 0 {
            return false;
        }

        if self
            .state
            .compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        unsafe {
            f(&mut *self.data.get());
        }

        self.state.store(0, Ordering::Release);
        true
    }
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        let prev = self.lock.state.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0);
    }
}

// ---------------------------------------------------------

pub trait Cache<K, V> {
    fn get<Q>(&self, hash: u64, q: &Q) -> V
    where
        Q: Borrow<K> + Hash;

    fn insert(&self, hash: u64, key: K, value: V) -> bool;
}

pub struct LockFreeCache<K, V> {
    entries: Box<[SpinRwLock<(K, V)>]>,
    size_exp: usize,
}

impl<K: Default, V: Idx> LockFreeCache<K, V> {
    pub fn with_capacity(cap: usize) -> Self {
        let size_exp = cap.isolate_highest_one().trailing_zeros() + 1;
        let entries = (0..(1 << size_exp))
            .map(|_| SpinRwLock::new(Default::default()))
            .collect::<Vec<SpinRwLock<(K, V)>>>()
            .into_boxed_slice();
        LockFreeCache {
            entries,
            size_exp: size_exp as usize,
        }
    }
}

impl<K: Hash + Eq, V: Idx> Cache<K, V> for LockFreeCache<K, V> {
    fn get<Q>(&self, hash: u64, q: &Q) -> V
    where
        Q: Borrow<K> + Hash,
    {
        let idx = hash & ((1 << self.size_exp) - 1);
        let read_guard = self.entries[idx as usize].read();
        if read_guard.0 == *q.borrow() {
            return read_guard.1;
        }

        V::NULL
    }

    fn insert(&self, hash: u64, key: K, value: V) -> bool {
        let idx = hash & ((1 << self.size_exp) - 1);
        let lock = &self.entries[idx as usize];
        // no thread is reading, it write
        lock.try_write_once(|pair| {
            pair.0 = key;
            pair.1 = value;
        })
    }
}
