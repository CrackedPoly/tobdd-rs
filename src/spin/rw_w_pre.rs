use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(feature = "op_stat")]
pub struct RwLockStat {
    pub read_acquires: AtomicUsize,
    pub read_cas_retries: AtomicUsize,
    pub read_writer_spins: AtomicUsize,
}

#[cfg(feature = "op_stat")]
impl Default for RwLockStat {
    fn default() -> Self {
        Self {
            read_acquires: AtomicUsize::new(0),
            read_cas_retries: AtomicUsize::new(0),
            read_writer_spins: AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "op_stat")]
impl std::fmt::Display for RwLockStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let acquires = self.read_acquires.load(Ordering::Relaxed);
        let cas_retries = self.read_cas_retries.load(Ordering::Relaxed);
        let writer_spins = self.read_writer_spins.load(Ordering::Relaxed);
        write!(
            f,
            "read_acquires: {}, read_cas_retries: {}, read_writer_spins: {} (retry_rate: {:.4}%)",
            acquires,
            cas_retries,
            writer_spins,
            if acquires > 0 {
                (cas_retries as f64 / acquires as f64) * 100.0
            } else {
                0.0
            }
        )
    }
}

const WRITE_LOCKED: usize = 1 << 0;
const WRITE_PENDING: usize = 1 << 1;

const READER_SHIFT: usize = 2;
const READER_ONE: usize = 1 << READER_SHIFT;

pub struct SpinRwLock<T> {
    state: AtomicUsize,
    value: UnsafeCell<T>,
    #[cfg(feature = "op_stat")]
    pub stat: RwLockStat,
}

pub struct ReadGuard<'a, T> {
    lock: &'a SpinRwLock<T>,
    _marker: PhantomData<&'a T>,
}

pub struct WriteGuard<'a, T> {
    lock: &'a SpinRwLock<T>,
    // 让 WriteGuard 不是 Sync（避免 &WriteGuard 在多线程共享导致“伪共享可变访问”语义）
    _marker: PhantomData<&'a mut T>,
}

impl<T: Default> Default for SpinRwLock<T> {
    fn default() -> Self {
        Self {
            state: Default::default(),
            value: Default::default(),
            #[cfg(feature = "op_stat")]
            stat: RwLockStat::default(),
        }
    }
}

impl<T> SpinRwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            value: UnsafeCell::new(value),
            #[cfg(feature = "op_stat")]
            stat: RwLockStat::default(),
        }
    }

    /// 读锁：无写者(locked/pending)才允许增加读者计数，否则自旋等待写者完成
    pub fn read(&self) -> ReadGuard<'_, T> {
        loop {
            let s = self.state.load(Ordering::Acquire);

            // 有写者（正在写 or 已占坑准备写） => 等待
            if (s & (WRITE_LOCKED | WRITE_PENDING)) != 0 {
                #[cfg(feature = "op_stat")]
                self.stat.read_writer_spins.fetch_add(1, Ordering::Relaxed);
                spin_loop();
                continue;
            }

            // 尝试 +1 读者（CAS 确保期间没被写者抢占 pending）
            let new = s.wrapping_add(READER_ONE);
            if self
                .state
                .compare_exchange_weak(s, new, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                #[cfg(feature = "op_stat")]
                self.stat.read_acquires.fetch_add(1, Ordering::Relaxed);
                return ReadGuard {
                    lock: self,
                    _marker: PhantomData,
                };
            }

            #[cfg(feature = "op_stat")]
            self.stat.read_cas_retries.fetch_add(1, Ordering::Relaxed);
            spin_loop();
        }
    }

    /// try_write：只要“没有其他写者”，就占坑并等待读者清空后进入写。
    /// 若已有写者（pending 或 locked），直接返回 None。
    pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        // 1) 先占写者坑位（WRITE_PENDING），用来阻止新读者进入
        let mut s = self.state.load(Ordering::Acquire);
        loop {
            if (s & (WRITE_LOCKED | WRITE_PENDING)) != 0 {
                return None; // 只有一个写者：已有人占坑或正在写 => 失败
            }

            let desired = s | WRITE_PENDING;
            match self
                .state
                .compare_exchange_weak(s, desired, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break,    // 成功占坑
                Err(ns) => s = ns, // 继续重试
            }
            spin_loop();
        }

        // 2) 等待读者计数清零，然后把 pending -> locked
        loop {
            let s = self.state.load(Ordering::Acquire);
            let readers = s >> READER_SHIFT;

            if readers == 0 {
                // 清 pending，置 locked
                let desired = (s & !WRITE_PENDING) | WRITE_LOCKED;

                if self
                    .state
                    .compare_exchange_weak(s, desired, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(WriteGuard {
                        lock: self,
                        _marker: PhantomData,
                    });
                }
            }

            spin_loop();
        }
    }
}

// ===== Guard implementations =====

impl<'a, T> Deref for ReadGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // 安全性：持有读锁时只读；写锁会阻止并等待所有读者退出
        unsafe { &*self.lock.value.get() }
    }
}

impl<'a, T> Drop for ReadGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.state.fetch_sub(READER_ONE, Ordering::Release);
    }
}

impl<'a, T> Deref for WriteGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<'a, T> DerefMut for WriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        // 安全性：WriteGuard 只在 WRITE_LOCKED 时创建，且同一时刻最多一个写者
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<'a, T> Drop for WriteGuard<'a, T> {
    fn drop(&mut self) {
        // 写结束：清 locked（顺手也清 pending，保证状态干净）
        self.lock
            .state
            .fetch_and(!(WRITE_LOCKED | WRITE_PENDING), Ordering::Release);
    }
}

// ===== Send/Sync =====
// lock 可在线程间传递：T 必须 Send
unsafe impl<T: Send> Send for SpinRwLock<T> {}
// lock 可并发共享：读共享要求 T: Sync，写独占要求 T: Send
unsafe impl<T: Send + Sync> Sync for SpinRwLock<T> {}
