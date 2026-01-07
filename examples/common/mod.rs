use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use tobdd::{Bdd, BddManager, BddOp, Manager};

pub type DefaultManager = Manager<usize, ()>;

#[derive(Default)]
pub struct AtomicOpCounts {
    and: AtomicUsize,
    or: AtomicUsize,
    comp: AtomicUsize,
}

impl AtomicOpCounts {
    pub fn record_and(&self) {
        self.and.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_or(&self) {
        self.or.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> OpSnapshot {
        OpSnapshot {
            and: self.and.load(Ordering::Relaxed),
            or: self.or.load(Ordering::Relaxed),
            comp: self.comp.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct OpSnapshot {
    pub and: usize,
    pub or: usize,
    pub comp: usize,
}

#[derive(Clone, Copy)]
pub struct BddRef {
    pub id: Bdd,
    pub refed: bool,
}

impl BddRef {
    fn new(id: Bdd, refed: bool) -> Self {
        Self { id, refed }
    }
}

pub struct BuildResult {
    pub root: BddRef,
    pub constraints: usize,
}

enum ConstraintKind {
    ExactlyOne,
    AtMostOne,
}

struct Constraint {
    kind: ConstraintKind,
    vars: Vec<usize>,
}

pub fn build_n_queens(manager: &DefaultManager, n: usize, counter: &AtomicOpCounts) -> BuildResult {
    let mut acc = BddRef::new(manager.get_true(), false);
    let mut constraints = 0usize;

    for row in 0..n {
        let vars = row_vars(n, row);
        let constraint = exactly_one(manager, &vars, counter);
        constraints += 1;
        acc = and_inplace(manager, acc, constraint, counter);
    }

    for col in 0..n {
        let vars = col_vars(n, col);
        let constraint = at_most_one(manager, &vars, counter);
        constraints += 1;
        acc = and_inplace(manager, acc, constraint, counter);
    }

    let max_diag = n as isize - 1;
    for diag in -max_diag..=max_diag {
        let vars = diag_vars(n, diag);
        if vars.len() >= 2 {
            let constraint = at_most_one(manager, &vars, counter);
            constraints += 1;
            acc = and_inplace(manager, acc, constraint, counter);
        }
    }

    for sum in 0..=(2 * n - 2) {
        let vars = anti_diag_vars(n, sum);
        if vars.len() >= 2 {
            let constraint = at_most_one(manager, &vars, counter);
            constraints += 1;
            acc = and_inplace(manager, acc, constraint, counter);
        }
    }

    BuildResult {
        root: acc,
        constraints,
    }
}

pub fn build_n_queens_mt(
    manager: &DefaultManager,
    n: usize,
    threads: usize,
    counter: &AtomicOpCounts,
) -> BuildResult {
    let jobs = build_jobs(n);
    let constraints = jobs.len();
    if constraints == 0 {
        return BuildResult {
            root: BddRef::new(manager.get_true(), false),
            constraints: 0,
        };
    }

    let threads = threads.clamp(1, constraints);
    let mut results = parallel_build_constraints(manager, &jobs, threads, counter);
    results = parallel_reduce(manager, results, threads, counter);

    let acc = results
        .pop()
        .unwrap_or_else(|| BddRef::new(manager.get_true(), false));

    BuildResult {
        root: acc,
        constraints,
    }
}

pub fn default_node_num(var_num: usize) -> usize {
    let base = var_num.saturating_mul(800);
    base.max(50_000)
}

fn build_jobs(n: usize) -> Vec<Constraint> {
    let mut jobs = Vec::new();

    for row in 0..n {
        jobs.push(Constraint {
            kind: ConstraintKind::ExactlyOne,
            vars: row_vars(n, row),
        });
    }

    for col in 0..n {
        jobs.push(Constraint {
            kind: ConstraintKind::AtMostOne,
            vars: col_vars(n, col),
        });
    }

    let max_diag = n as isize - 1;
    for diag in -max_diag..=max_diag {
        let vars = diag_vars(n, diag);
        if vars.len() >= 2 {
            jobs.push(Constraint {
                kind: ConstraintKind::AtMostOne,
                vars,
            });
        }
    }

    for sum in 0..=(2 * n - 2) {
        let vars = anti_diag_vars(n, sum);
        if vars.len() >= 2 {
            jobs.push(Constraint {
                kind: ConstraintKind::AtMostOne,
                vars,
            });
        }
    }

    jobs
}

fn parallel_build_constraints(
    manager: &DefaultManager,
    jobs: &[Constraint],
    threads: usize,
    counter: &AtomicOpCounts,
) -> Vec<BddRef> {
    let next = AtomicUsize::new(0);
    let mut results = Vec::with_capacity(jobs.len());

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let manager = manager;
            let counter = counter;
            let next = &next;
            handles.push(scope.spawn(move || {
                let mut local = Vec::new();
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= jobs.len() {
                        break;
                    }
                    let job = &jobs[idx];
                    let bdd = match job.kind {
                        ConstraintKind::ExactlyOne => exactly_one(manager, &job.vars, counter),
                        ConstraintKind::AtMostOne => at_most_one(manager, &job.vars, counter),
                    };
                    local.push(bdd);
                }
                local
            }));
        }

        for handle in handles {
            let local = handle.join().expect("worker thread panicked");
            results.extend(local);
        }
    });

    results
}

fn parallel_reduce(
    manager: &DefaultManager,
    mut inputs: Vec<BddRef>,
    threads: usize,
    counter: &AtomicOpCounts,
) -> Vec<BddRef> {
    if inputs.len() <= 1 {
        return inputs;
    }

    while inputs.len() > 1 {
        let round_threads = threads.clamp(1, inputs.len());
        let chunk_size = (inputs.len() + round_threads - 1) / round_threads;
        let mut chunks = Vec::with_capacity(round_threads);

        while !inputs.is_empty() {
            let take = chunk_size.min(inputs.len());
            let chunk: Vec<BddRef> = inputs.drain(0..take).collect();
            chunks.push(chunk);
        }

        let mut results = Vec::with_capacity(chunks.len());
        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                let manager = manager;
                let counter = counter;
                handles.push(scope.spawn(move || reduce_chunk(manager, chunk, counter)));
            }

            for handle in handles {
                let local = handle.join().expect("worker thread panicked");
                results.push(local);
            }
        });

        inputs = results;
    }

    inputs
}

fn reduce_chunk(
    manager: &DefaultManager,
    mut items: Vec<BddRef>,
    counter: &AtomicOpCounts,
) -> BddRef {
    let mut acc = items
        .pop()
        .unwrap_or_else(|| BddRef::new(manager.get_true(), false));
    while let Some(item) = items.pop() {
        acc = and_inplace(manager, acc, item, counter);
    }
    acc
}

fn exactly_one(manager: &DefaultManager, vars: &[usize], counter: &AtomicOpCounts) -> BddRef {
    let at_least = or_vars(manager, vars, counter);
    let at_most = at_most_one(manager, vars, counter);
    counter.record_and();
    let both = manager.and(at_least.id, at_most.id);
    manager.ref_bdd(both);
    if at_least.refed {
        manager.deref_bdd(at_least.id);
    }
    if at_most.refed {
        manager.deref_bdd(at_most.id);
    }
    BddRef::new(both, true)
}

fn at_most_one(manager: &DefaultManager, vars: &[usize], counter: &AtomicOpCounts) -> BddRef {
    if vars.len() < 2 {
        return BddRef::new(manager.get_true(), false);
    }

    let mut acc = BddRef::new(manager.get_true(), false);
    for i in 0..(vars.len() - 1) {
        let ni = manager.get_nvar(vars[i]);
        for j in (i + 1)..vars.len() {
            let nj = manager.get_nvar(vars[j]);
            counter.record_or();
            let clause = manager.or(ni, nj);
            manager.ref_bdd(clause);
            acc = and_inplace(manager, acc, BddRef::new(clause, true), counter);
        }
    }
    acc
}

fn or_vars(manager: &DefaultManager, vars: &[usize], counter: &AtomicOpCounts) -> BddRef {
    let mut acc = BddRef::new(manager.get_var(vars[0]), false);
    for &var in vars.iter().skip(1) {
        counter.record_or();
        let tmp = manager.or(acc.id, manager.get_var(var));
        manager.ref_bdd(tmp);
        if acc.refed {
            manager.deref_bdd(acc.id);
        }
        acc = BddRef::new(tmp, true);
    }
    acc
}

fn and_inplace(
    manager: &DefaultManager,
    lhs: BddRef,
    rhs: BddRef,
    counter: &AtomicOpCounts,
) -> BddRef {
    counter.record_and();
    let tmp = manager.and(lhs.id, rhs.id);
    manager.ref_bdd(tmp);
    if lhs.refed {
        manager.deref_bdd(lhs.id);
    }
    if rhs.refed {
        manager.deref_bdd(rhs.id);
    }
    BddRef::new(tmp, true)
}

fn row_vars(n: usize, row: usize) -> Vec<usize> {
    let mut vars = Vec::with_capacity(n);
    for col in 0..n {
        vars.push(var_index(n, row, col));
    }
    vars
}

fn col_vars(n: usize, col: usize) -> Vec<usize> {
    let mut vars = Vec::with_capacity(n);
    for row in 0..n {
        vars.push(var_index(n, row, col));
    }
    vars
}

fn diag_vars(n: usize, diag: isize) -> Vec<usize> {
    let mut vars = Vec::with_capacity(n);
    for row in 0..n {
        let col = row as isize - diag;
        if col >= 0 && col < n as isize {
            vars.push(var_index(n, row, col as usize));
        }
    }
    vars
}

fn anti_diag_vars(n: usize, sum: usize) -> Vec<usize> {
    let mut vars = Vec::with_capacity(n);
    for row in 0..n {
        if sum >= row {
            let col = sum - row;
            if col < n {
                vars.push(var_index(n, row, col));
            }
        }
    }
    vars
}

fn var_index(n: usize, row: usize, col: usize) -> usize {
    row * n + col
}
