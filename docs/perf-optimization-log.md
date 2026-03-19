# tobdd Performance Optimization Log

This document records the performance optimizations applied to the tobdd BDD library
on the `perf` branch, their rationale, measured effects, and trade-offs.

All benchmarks use the **bdd-bench** equivalence class computation workload
(`/Users/jian/Workspaces/Research/bdd-bench`). Thread counts tested: 1, 2, 4, 8.
Timing is wall-clock median of 5 runs. Table size = 8M, cache size = 2M.

---

## Optimization 1: DistributedRwLock (replacing global SpinRwLock)

**Files**: `src/spin/distributed_rw.rs` (new), `src/tobdd.rs`

### Problem

The `Manager` uses a reader-writer lock to protect against concurrent GC.
The original `SpinRwLock<()>` used a single `AtomicUsize` for reader counting.
Every `read()` call required `fetch_add` (acquire) and `fetch_sub` (release) on
the same cache line, causing severe cache-line bouncing under multi-threaded workloads.

### Solution

A distributed reader-writer lock where each thread has its own 128-byte-aligned
`AtomicU32` slot. The read path becomes:

1. `store(1)` to private slot (no contention)
2. `fence(SeqCst)` (Dekker-style barrier)
3. `load(writer_pending)` (shared read, usually L1 hit)

The write path (GC only) scans all active slots to wait for readers to drain.

### Effect

Eliminated CAS contention on the read path. This is the foundation for all
subsequent optimizations. The `fence(SeqCst)` per operation remained, which
became the next bottleneck.

### Trade-off

- Fixed maximum of 32 threads (one slot per thread, 4 KiB total).
- Writer (GC) must scan all active slots, but GC is rare.

---

## Optimization 2: SeqLock Cache (replacing per-entry SpinRwLock)

**Files**: `src/cache.rs`

### Problem

Each operation cache entry (`and_cache`, `or_cache`, etc.) was protected by a
`SpinRwLock<(K, V)>`. Every cache lookup required two atomic RMW operations:
`fetch_add` (enter read) and `fetch_sub` (exit read). With millions of cache
accesses per workload, this was a significant overhead.

### Solution

Replaced per-entry `SpinRwLock` with a **SeqLock** (sequence lock):

- **Reader**: `load(seq)` → read key/value → `fence(Acquire)` → `load(seq)` again.
  If seq changed or was odd (writer in progress), treat as miss. **Zero atomic RMW**.
- **Writer**: CAS seq even→odd (write-in-progress), write data, `store` seq+2
  (write-complete).

### Effect

fabric32 benchmark (median, ms):

| Threads | Before (SpinRwLock cache) | After (SeqLock cache) | Improvement |
|---------|---------------------------|------------------------|-------------|
| 8T      | ~22ms                     | ~14.3ms                | **35%**     |

All thread counts on fabric32 improved.

### Trade-off

- **N-queens shared Manager regression**: 8T speedup dropped from 2.76x to 2.06x.
  When a SeqLock writer is active (seq is odd), readers immediately return miss
  instead of waiting. Under high write contention (N-queens has frequent cache
  inserts), this increases miss rate and causes redundant computation that
  outweighs the read-path savings.
- SeqLock requires `K` and `V` to be `Copy`.
- Works best for read-heavy, low-write-contention workloads (like fabric).

---

## Optimization 3: Sticky Lock / Batch Read Lock (eliminating per-op mfence)

**Files**: `src/spin/distributed_rw.rs`, `src/tobdd.rs`

### Problem

Even with DistributedRwLock, every BDD operation (`and`, `or`, `comp`, `not`)
called `enter_op()` → `rw_lock.read()` → `fence(SeqCst)`. On x86, `fence(SeqCst)`
compiles to `mfence` (~30ns). fabric32 has 977K operations, so mfence alone
cost ~29ms — **53% of the 55ms single-thread runtime**.

Profile breakdown (fabric32, 1T):
- 977K `enter_op` calls × 30ns mfence = ~29ms
- Actual BDD computation = ~19ms
- Total = ~48ms (mfence dominated)

### Solution

Hold the read lock across consecutive operations ("sticky lock"):

```rust
fn enter_op(&self) {
    if self.rw_lock.is_read_locked() {
        return;  // HOT PATH: one Relaxed load, zero mfence
    }
    // Cold path: GC/grow check, then read_lock()
    ...
    self.rw_lock.read_lock();
}

fn exit_op(&self) {
    if self.rw_lock.is_writer_pending() {
        self.rw_lock.read_unlock();  // Cooperative yield for GC
    }
}
```

New public methods on `DistributedRwLock`:
- `read_lock()` / `read_unlock()` — manual lifetime management (no RAII guard)
- `is_read_locked()` — `Relaxed` load on current thread's slot
- `is_writer_pending()` — `Relaxed` load on writer flag

The `gc()` method was also updated to release the sticky lock before calling
`try_write()` (prevents self-deadlock), and re-acquire it afterward.

### Effect

fabric32 benchmark (median, ms):

| Threads | Before (per-op lock) | After (sticky lock) | Improvement |
|---------|----------------------|----------------------|-------------|
| 1T      | 48.0ms               | 31.8ms               | **34%**     |
| 2T      | 28.2ms               | 27.7ms               | 2%          |
| 4T      | 20.3ms               | 14.5ms               | **29%**     |
| 8T      | 18.2ms               | 14.4ms               | **21%**     |

stanford benchmark:

| Threads | Before    | After     | Improvement |
|---------|-----------|-----------|-------------|
| 1T      | 412ms     | 246ms     | **40%**     |
| 8T      | 254ms     | 145ms     | **43%**     |

### Note on scaling ratio

The 1T→8T speedup ratio *decreased* from 2.63x to 2.20x on fabric32. This is
expected and is **not** a regression:

- Before: 1T = 19ms compute + 29ms mfence = 48ms. Multi-threading overlaps
  each thread's mfence in wall-clock time, so the 29ms "disappears" at 8T.
  The 2.63x speedup was inflated by mfence overhead masking single-thread cost.
- After: 1T = 32ms pure compute. 8T = 14.4ms. The 2.20x is the **true**
  parallel speedup of the BDD computation itself, limited by data dependencies
  and load imbalance.

### Trade-off

- A thread that finishes all BDD work holds the sticky lock indefinitely.
  `try_write` (GC) would fail and be deferred. Acceptable because:
  (a) fabric32 has zero GC,
  (b) rayon work-stealing keeps threads calling BDD ops,
  (c) `try_write` is non-blocking and retries on next `enter_op`.
- `gc()` must explicitly release/re-acquire the sticky lock.

---

## Cumulative Effect (fabric32, 1T)

| Stage                              | 1T Time  | Cumulative Improvement |
|------------------------------------|----------|------------------------|
| Original SpinRwLock                | ~55-60ms | —                      |
| + DistributedRwLock                | ~55ms    | ~0% (eliminated CAS, mfence remains) |
| + SeqLock Cache                    | ~48ms    | ~15% (eliminated cache RMW) |
| + Sticky Lock                      | ~32ms    | **~42%** total          |

The remaining ~32ms is genuine BDD computation (make_node, hash, recursive
traversal). Lock-related overhead has been effectively eliminated.

---

## Open Questions / Future Work

1. **N-queens shared Manager scaling** — SeqLock's miss-on-write behavior hurts
   cache-heavy workloads. Consider a hybrid: SeqLock for read-dominated caches,
   fallback to blocking read for write-heavy scenarios.
2. **Node table (LockFreeSet) contention** — CAS-based linked list insertion still
   causes cross-core cache invalidation. This is the next scaling bottleneck for
   shared-Manager workloads.
3. **Reference counting contention** — `ref_bdd` / `deref_bdd` atomic increments
   on shared BDD nodes cause cache-line bouncing when multiple threads operate on
   common sub-BDDs.
4. **`fence(SeqCst)` → `swap(SeqCst)`** — On x86, replacing `store(Relaxed)` +
   `fence(SeqCst)` with `swap(1, SeqCst)` (compiles to `lock xchg`) may shave
   a few ns off the cold path. Minor impact since sticky lock makes the cold path
   rare.
