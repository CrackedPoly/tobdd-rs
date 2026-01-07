use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tobdd::{Bdd, BddManager, BddOp, Manager};

type DefaultManager = Manager;

fn bench_n_queens(c: &mut Criterion) {
    let sizes = env_sizes("NQUEENS_SIZES", &[10]);
    let fixed_node_num = env_usize_opt("NQUEENS_NODE_NUM");
    let fixed_cache_size = env_usize_opt("NQUEENS_CACHE_SIZE");

    let mut group = c.benchmark_group("n_queens_bdd");
    group.sample_size(10);

    for n in sizes {
        let var_num = n * n;
        let node_num = fixed_node_num.unwrap_or_else(|| default_node_num(var_num));
        let cache_size = fixed_cache_size.unwrap_or(node_num / 2);

        group.bench_with_input(BenchmarkId::new("n", n), &n, |b, &n| {
            b.iter(|| {
                let manager: DefaultManager = DefaultManager::init(node_num, cache_size, var_num);
                let root = build_n_queens(&manager, n);
                black_box(root.id);
                if root.refed {
                    manager.deref_bdd(root.id);
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_n_queens);
criterion_main!(benches);

#[derive(Clone, Copy)]
struct BddRef {
    id: Bdd,
    refed: bool,
}

impl BddRef {
    fn new(id: Bdd, refed: bool) -> Self {
        Self { id, refed }
    }
}

fn build_n_queens(manager: &DefaultManager, n: usize) -> BddRef {
    let mut acc = BddRef::new(manager.get_true(), false);

    for row in 0..n {
        let vars = row_vars(n, row);
        let constraint = exactly_one(manager, &vars);
        acc = and_inplace(manager, acc, constraint);
    }

    for col in 0..n {
        let vars = col_vars(n, col);
        let constraint = at_most_one(manager, &vars);
        acc = and_inplace(manager, acc, constraint);
    }

    let max_diag = n as isize - 1;
    for diag in -max_diag..=max_diag {
        let vars = diag_vars(n, diag);
        if vars.len() >= 2 {
            let constraint = at_most_one(manager, &vars);
            acc = and_inplace(manager, acc, constraint);
        }
    }

    for sum in 0..=(2 * n - 2) {
        let vars = anti_diag_vars(n, sum);
        if vars.len() >= 2 {
            let constraint = at_most_one(manager, &vars);
            acc = and_inplace(manager, acc, constraint);
        }
    }

    acc
}

fn exactly_one(manager: &DefaultManager, vars: &[usize]) -> BddRef {
    let at_least = or_vars(manager, vars);
    let at_most = at_most_one(manager, vars);
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

fn at_most_one(manager: &DefaultManager, vars: &[usize]) -> BddRef {
    if vars.len() < 2 {
        return BddRef::new(manager.get_true(), false);
    }

    let mut acc = BddRef::new(manager.get_true(), false);
    for i in 0..(vars.len() - 1) {
        let ni = manager.get_nvar(vars[i]);
        for j in (i + 1)..vars.len() {
            let nj = manager.get_nvar(vars[j]);
            let clause = manager.or(ni, nj);
            manager.ref_bdd(clause);
            acc = and_inplace(manager, acc, BddRef::new(clause, true));
        }
    }
    acc
}

fn or_vars(manager: &DefaultManager, vars: &[usize]) -> BddRef {
    let mut acc = BddRef::new(manager.get_var(vars[0]), false);
    for &var in vars.iter().skip(1) {
        let tmp = manager.or(acc.id, manager.get_var(var));
        manager.ref_bdd(tmp);
        if acc.refed {
            manager.deref_bdd(acc.id);
        }
        acc = BddRef::new(tmp, true);
    }
    acc
}

fn and_inplace(manager: &DefaultManager, lhs: BddRef, rhs: BddRef) -> BddRef {
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

fn default_node_num(var_num: usize) -> usize {
    let base = var_num.saturating_mul(800);
    base.max(50_000)
}

fn env_sizes(name: &str, default: &[usize]) -> Vec<usize> {
    match std::env::var(name) {
        Ok(value) => {
            let mut sizes: Vec<usize> = value
                .split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .collect();
            if sizes.is_empty() {
                sizes = default.to_vec();
            }
            sizes
        }
        Err(_) => default.to_vec(),
    }
}

fn env_usize_opt(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}
