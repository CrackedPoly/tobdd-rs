use std::sync::atomic::{AtomicIsize, Ordering};

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
    pub const fn new(value: T) -> Self {
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

    /// Blocking write - waits until all readers finish
    #[inline]
    pub fn write<F>(&self, f: F)
    where
        F: FnOnce(&mut T),
    {
        loop {
            let s = self.state.load(Ordering::Acquire);
            if s == 0 {
                if self
                    .state
                    .compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
            spin_loop();
        }

        unsafe {
            f(&mut *self.data.get());
        }

        self.state.store(0, Ordering::Release);
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

