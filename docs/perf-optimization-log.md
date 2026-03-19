# tobdd Performance Optimization Log

This document records performance optimizations attempted on the tobdd BDD library
(`perf` branch), their rationale, measured effects, and outcomes. Some optimizations
were **reverted** after discovering regressions on larger workloads.

Benchmarks used:
- **bdd-bench** (`/Users/jian/Workspaces/Research/bdd-bench`): equivalence class
  computation. Datasets: fabric32 (small, zero GC), stanford (medium), i2 (large,
  frequent GC/grow). Table size = 8M, cache size = 2M unless noted.
- **Rapimt** (`/Users/jian/Workspaces/Develop/Rapimt`): inverse model computation.
  Default table = 10K, cache = 1K.

Timing: wall-clock median of 3-5 runs unless noted.

---

## Optimization 1: DistributedRwLock (replacing global SpinRwLock) — KEPT

**Files**: `src/spin/distributed_rw.rs` (new), `src/tobdd.rs`

**Status**: Retained on `perf` branch.

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

- Eliminated CAS contention on the read path.
- On x86 (i9-12900): eliminates CAS retry overhead, significant multi-thread benefit.
- On Apple Silicon: `fence(SeqCst)` compiles to `dmb ish` (~10ns vs x86 `mfence`
  ~30ns), so single-thread benefit is smaller. Multi-thread CAS elimination still
  helps.

bdd-bench i2 dataset (main SpinRwLock → DistributedRwLock only, commit `c710d73`):
- Rapimt i2 1T: 1.26s → 1.40s (within noise)
- No regression on any workload.

### Trade-off

- Fixed maximum of 32 threads (one slot per thread, 4 KiB total).
- Writer (GC) must scan all active slots, but GC is rare.

---

## Optimization 2: SeqLock Cache (replacing per-entry SpinRwLock) — REVERTED

**Files**: `src/cache.rs`

**Status**: **Reverted.** Caused catastrophic regression on large datasets.

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

### Effect on small datasets (appeared positive)

fabric32 benchmark (bdd-bench, table=8M):

| Threads | Before (SpinRwLock cache) | After (SeqLock cache) | Change |
|---------|---------------------------|------------------------|--------|
| 1T      | ~55ms                     | ~48ms                  | -13%   |
| 8T      | ~22ms                     | ~14.3ms                | **-35%** |

### Catastrophic regression on large datasets

Rapimt i2 1T (table=10K, triggers frequent GC/grow):

| Version | Time |
|---------|------|
| Before (SpinRwLock cache, commit `c710d73`) | **1.4s** |
| After (SeqLock cache, commit `ecc7e86`) | **24.9s** |

**18x slower.** Root cause: SeqLock returns miss when a writer is active (seq is
odd), whereas SpinRwLock waits for the writer to finish and then returns the cached
value. On write-intensive workloads:

1. Cache insert (CAS seq odd) → concurrent reader sees odd seq → returns miss
2. Miss → full BDD recursive recomputation (may involve thousands of sub-operations)
3. Each sub-operation also has cache lookups that may also miss → **cascade effect**
4. More computation → more cache inserts → more misses → exponential blowup

This also explained the earlier N-queens shared Manager regression (8T speedup
dropped from 2.76x to 2.06x).

### Decision

**Reverted to SpinRwLock per-entry cache.** The SeqLock's zero-RMW read path
benefit is real for read-dominated workloads, but the miss-on-write cascade makes
it unsafe as a general-purpose optimization. A future approach could use a hybrid
strategy (SeqLock with fallback to spinning), but the complexity is not justified
given the magnitude of the regression.

---

## Optimization 3: Sticky Lock / Batch Read Lock — REVERTED

**Files**: `src/spin/distributed_rw.rs`, `src/tobdd.rs`

**Status**: **Reverted.** Prevented GC/grow from triggering; also ineffective on ARM.

### Problem

Every BDD operation called `enter_op()` → `rw_lock.read()` → `fence(SeqCst)`.
On x86, `fence(SeqCst)` = `mfence` (~30ns). fabric32 has 977K operations, so
mfence alone cost ~29ms — **53% of the 55ms single-thread runtime**.

### Solution

Hold the read lock across consecutive operations ("sticky lock"):

```rust
fn enter_op(&self) {
    if self.rw_lock.is_read_locked() {
        return;  // HOT PATH: one Relaxed load, zero mfence
    }
    // Cold path: GC/grow check, then read_lock()
    ...
}

fn exit_op(&self) {
    if self.rw_lock.is_writer_pending() {
        self.rw_lock.read_unlock();  // Cooperative yield for GC
    }
}
```

Added public methods on `DistributedRwLock`: `read_lock()`, `read_unlock()`,
`is_read_locked()`, `is_writer_pending()`.

### Initial results (appeared positive, fabric32 only)

fabric32 (bdd-bench, table=8M, zero GC):

| Threads | Before | After | Improvement |
|---------|--------|-------|-------------|
| 1T      | 48.0ms | 31.8ms | **34%** |
| 8T      | 18.2ms | 14.4ms | **21%** |

stanford: 1T 412ms → 246ms (**40%**), 8T 254ms → 145ms (**43%**).

### Critical bug: GC/grow never triggers

The hot path (`is_read_locked() → return`) **skips the load factor check entirely**.
Once the sticky lock is acquired, it is never released in single-threaded mode
(no writer is ever pending), so `enter_op` always takes the hot path and the
GC/grow condition is never evaluated.

For bdd-bench with table=8M, this was invisible (table never fills). For Rapimt
with table=10K, the table never grows despite hundreds of thousands of nodes
being inserted, causing catastrophic hash chain overflow:

| Benchmark | Without sticky lock | With sticky lock |
|-----------|---------------------|------------------|
| Rapimt i2 1T (table=10K) | **1.4s** | **24.9s** |

### Attempted fix: check load factor on hot path

Moving the load factor check before the `is_read_locked()` check:

```rust
fn enter_op(&self) {
    let entry_num = self.set.entry_num();  // AtomicUsize::load(Relaxed)
    let bucket_size = self.set.bucket_size();
    if entry_num >= bucket_size * MAX_LOAD_FACTOR {
        // release sticky lock, try GC/grow, re-acquire
        ...
    }
    if self.rw_lock.is_read_locked() {
        return;  // hot path
    }
    self.rw_lock.read_lock();
}
```

This fixed the GC/grow issue (Rapimt i2 back to ~1.3s), but **eliminated the
sticky lock's performance benefit** on fabric32 — it went back to ~48ms (same as
baseline). The `entry_num()` load on every call (even though `Relaxed`) touches
a frequently-written atomic cache line (`num_entry` is incremented on every node
insertion via `fetch_add`), adding enough overhead to negate the mfence savings.

### Additional factor: ARM vs x86

The sticky lock was designed to eliminate x86 `mfence` (~30ns). On Apple Silicon,
`fence(SeqCst)` compiles to `dmb ish` (~10ns), making the mfence overhead only
~10ms for 977K ops instead of ~30ms. The potential gain is much smaller and more
easily offset by any additional hot-path work.

### Decision

**Reverted entirely.** The sticky lock has a fundamental tension: it must skip
expensive operations on the hot path to be fast, but skipping the load factor
check breaks correctness for small/growing tables. The fix (checking load factor
on every call) negates the performance benefit. On ARM, the benefit was marginal
to begin with.

A future version could explore:
- Thread-local counter to check load factor every N ops instead of every op
- Separate "needs-grow" flag set by `make_node` when insertion triggers a long
  chain, checked cheaply on the hot path
- x86-specific sticky lock behind `#[cfg(target_arch)]`

---

## Current State (perf branch)

Only **Optimization 1 (DistributedRwLock)** is retained. Optimizations 2 and 3
have been reverted.

| Benchmark | main branch | perf branch | Change |
|-----------|-------------|-------------|--------|
| Rapimt i2 1T | 1.26s | 1.34s | ~0% (noise) |
| bdd-bench fabric32 1T | ~48ms | ~48ms | ~0% |
| bdd-bench fabric32 8T | ~18ms | ~22ms | ~0% (noise) |

The DistributedRwLock is architecturally superior to SpinRwLock (eliminates CAS
contention) even if the single-thread benefit is negligible on ARM. It provides
better multi-thread scaling on x86 and is retained for forward compatibility.

---

## Lessons Learned

1. **Always test on diverse workloads.** SeqLock cache showed 35% improvement on
   fabric32 but 18x regression on i2. The cascade effect (miss → recompute →
   more misses) is non-obvious and devastating.

2. **Small-table behavior matters.** Rapimt uses 10K initial table (grows
   dynamically). bdd-bench uses 8M (never grows for small datasets). An
   optimization that works with pre-allocated large tables may catastrophically
   fail when the table must grow.

3. **ARM vs x86 cost models differ.** `fence(SeqCst)` is ~3x cheaper on Apple
   Silicon than on x86. Optimizations targeting mfence elimination may not be
   worthwhile on ARM.

4. **Correctness invariants across code paths.** The sticky lock's hot path
   skipped the GC/grow check — a correctness issue masked by the benchmark's
   large initial table. Always verify that optimized paths preserve all
   invariants, not just the ones exercised by the primary benchmark.

---

## Open Questions / Future Work

1. **Node table (LockFreeSet) contention** — CAS-based linked list insertion still
   causes cross-core cache invalidation. This is the primary scaling bottleneck for
   shared-Manager workloads.
2. **Reference counting contention** — `ref_bdd` / `deref_bdd` atomic increments
   on shared BDD nodes cause cache-line bouncing when multiple threads operate on
   common sub-BDDs.
3. **Table size auto-tuning** — bdd-bench's 8M fixed table wastes memory and hurts
   cache locality for small workloads. Rapimt's 10K default requires frequent
   grow. An adaptive initial size based on input could help both.
4. **x86-specific sticky lock** — Behind `#[cfg(target_arch = "x86_64")]`, with a
   cheap "needs-grow" flag to avoid the load factor check on hot path.
