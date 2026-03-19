use core::{
    hint::spin_loop,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering, fence},
};

#[cfg(feature = "op_stat")]
use super::rw_w_pre::RwLockStat;

/// Maximum number of threads supported. Each slot takes 128 bytes,
/// so 32 slots = 4 KiB total. Panics if exceeded.
const MAX_THREADS: usize = 32;

/// Cache-line-padded reader counter. On Apple Silicon the cache line is 128 bytes.
/// Each thread exclusively owns one slot, eliminating false sharing.
#[repr(align(128))]
struct ReaderSlot {
    count: AtomicU32,
}

impl ReaderSlot {
    const fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
        }
    }
}

/// Global counter for assigning thread-local slot indices.
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SLOT_INDEX: usize = {
        let idx = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        assert!(idx < MAX_THREADS, "DistributedRwLock: too many threads (max {MAX_THREADS})");
        idx
    };
}

/// A reader–writer lock optimized for read-heavy, low-writer workloads.
///
/// Instead of a single atomic counter that all readers CAS on (causing
/// cache-line bouncing), each thread has its own cache-line-padded slot.
/// The read path uses a plain `store` (no CAS) to its private slot, achieving
/// zero contention on the hot path.
pub struct DistributedRwLock {
    writer_pending: AtomicBool,
    writer_locked: AtomicBool,
    slots: [ReaderSlot; MAX_THREADS],
    /// Tracks the highest slot index ever seen, so the writer only
    /// needs to scan active slots.
    max_slot_seen: AtomicUsize,
    #[cfg(feature = "op_stat")]
    pub stat: RwLockStat,
}

unsafe impl Send for DistributedRwLock {}
unsafe impl Sync for DistributedRwLock {}

impl Default for DistributedRwLock {
    fn default() -> Self {
        // SAFETY: ReaderSlot is just an AtomicU32 with padding — zero-init is valid.
        const NEW_SLOT: ReaderSlot = ReaderSlot::new();
        Self {
            writer_pending: AtomicBool::new(false),
            writer_locked: AtomicBool::new(false),
            slots: [NEW_SLOT; MAX_THREADS],
            max_slot_seen: AtomicUsize::new(0),
            #[cfg(feature = "op_stat")]
            stat: RwLockStat::default(),
        }
    }
}

pub struct ReadGuard<'a> {
    lock: &'a DistributedRwLock,
    slot: usize,
}

pub struct WriteGuard<'a> {
    lock: &'a DistributedRwLock,
}

// ReadGuard derefs to () for API compatibility with SpinRwLock<()>
impl<'a> Deref for ReadGuard<'a> {
    type Target = ();
    fn deref(&self) -> &() {
        &()
    }
}

impl<'a> Drop for ReadGuard<'a> {
    fn drop(&mut self) {
        self.lock.slots[self.slot].count.store(0, Ordering::Release);
    }
}

impl<'a> Deref for WriteGuard<'a> {
    type Target = ();
    fn deref(&self) -> &() {
        &()
    }
}

impl<'a> Drop for WriteGuard<'a> {
    fn drop(&mut self) {
        // Clear writer_locked first, then writer_pending, both with Release
        // so readers see the updates.
        self.lock.writer_locked.store(false, Ordering::Release);
        self.lock.writer_pending.store(false, Ordering::Release);
    }
}

impl DistributedRwLock {
    /// Acquire a read lock. The hot path does zero CAS operations:
    ///
    /// 1. `store(1)` to this thread's private slot (no contention)
    /// 2. `fence(SeqCst)` — Dekker-style barrier
    /// 3. `load(writer_pending)` — shared read, usually L1 hit
    /// 4. If no writer → return ReadGuard. Done.
    /// 5. If writer pending → undo, spin, retry.
    pub fn read(&self) -> ReadGuard<'_> {
        let slot = SLOT_INDEX.with(|&idx| idx);

        // Update max_slot_seen so the writer knows how far to scan.
        // Relaxed is fine — worst case writer scans one extra slot.
        let cur_max = self.max_slot_seen.load(Ordering::Relaxed);
        if slot >= cur_max {
            // fetch_max isn't available on all targets; use a CAS loop.
            // This only runs once per thread per lock instance.
            let _ = self.max_slot_seen.fetch_max(slot + 1, Ordering::Relaxed);
        }

        loop {
            // 1. Announce ourselves as a reader.
            self.slots[slot].count.store(1, Ordering::Relaxed);

            // 2. Dekker barrier: ensures our slot write is visible before
            //    we read writer_pending, and vice versa for the writer.
            fence(Ordering::SeqCst);

            // 3. Check if a writer is pending.
            if !self.writer_pending.load(Ordering::Relaxed) {
                // Fast path — no writer, we're in.
                #[cfg(feature = "op_stat")]
                self.stat.read_acquires.fetch_add(1, Ordering::Relaxed);
                return ReadGuard { lock: self, slot };
            }

            // 4. Writer is pending — back off and retry.
            self.slots[slot].count.store(0, Ordering::Release);

            #[cfg(feature = "op_stat")]
            self.stat.read_writer_spins.fetch_add(1, Ordering::Relaxed);

            // Spin until writer is done.
            while self.writer_pending.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
    }

    /// Acquire read lock without returning a guard (caller manages lifetime).
    /// Uses the same Dekker protocol as `read()`.
    pub fn read_lock(&self) {
        let slot = SLOT_INDEX.with(|&idx| idx);

        let cur_max = self.max_slot_seen.load(Ordering::Relaxed);
        if slot >= cur_max {
            let _ = self.max_slot_seen.fetch_max(slot + 1, Ordering::Relaxed);
        }

        loop {
            self.slots[slot].count.store(1, Ordering::Relaxed);
            fence(Ordering::SeqCst);

            if !self.writer_pending.load(Ordering::Relaxed) {
                #[cfg(feature = "op_stat")]
                self.stat.read_acquires.fetch_add(1, Ordering::Relaxed);
                return;
            }

            self.slots[slot].count.store(0, Ordering::Release);

            #[cfg(feature = "op_stat")]
            self.stat.read_writer_spins.fetch_add(1, Ordering::Relaxed);

            while self.writer_pending.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
    }

    /// Release read lock for current thread.
    pub fn read_unlock(&self) {
        let slot = SLOT_INDEX.with(|&idx| idx);
        self.slots[slot].count.store(0, Ordering::Release);
    }

    /// Check if current thread holds the read lock.
    pub fn is_read_locked(&self) -> bool {
        let slot = SLOT_INDEX.with(|&idx| idx);
        self.slots[slot].count.load(Ordering::Relaxed) != 0
    }

    /// Check if a writer is waiting (cheap Relaxed load).
    pub fn is_writer_pending(&self) -> bool {
        self.writer_pending.load(Ordering::Relaxed)
    }

    /// Try to acquire the write lock. Returns `None` if another writer
    /// is already pending or locked.
    ///
    /// 1. CAS `writer_pending` false→true (uncontended — only one writer)
    /// 2. `fence(SeqCst)` — Dekker barrier
    /// 3. Scan all active slots, spin until all reader counts are zero
    /// 4. Set `writer_locked = true`
    /// 5. Return WriteGuard
    pub fn try_write(&self) -> Option<WriteGuard<'_>> {
        // 1. Try to claim writer_pending.
        if self
            .writer_pending
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None; // Another writer is active.
        }

        // 2. Dekker barrier: ensures writer_pending is visible to readers
        //    before we read their slots.
        fence(Ordering::SeqCst);

        // 3. Wait for all readers to finish.
        let max = self.max_slot_seen.load(Ordering::Acquire);
        loop {
            let mut all_clear = true;
            for i in 0..max {
                if self.slots[i].count.load(Ordering::Acquire) != 0 {
                    all_clear = false;
                    break;
                }
            }
            if all_clear {
                break;
            }
            spin_loop();
        }

        // 4. All readers drained — we own exclusive access.
        self.writer_locked.store(true, Ordering::Release);

        Some(WriteGuard { lock: self })
    }
}
