mod common;

use std::time::Instant;

use common::{AtomicOpCounts, DefaultManager, build_n_queens_mt, default_node_num};
use tobdd::BddManager;

fn main() {
    let mut args = Args::from_env();
    if args.help {
        print_help();
        return;
    }

    if args.n == 0 {
        eprintln!("n must be >= 1");
        std::process::exit(1);
    }

    let vars = args.n.checked_mul(args.n).unwrap_or_else(|| {
        eprintln!("n is too large");
        std::process::exit(1);
    });

    if args.node_num == 0 {
        args.node_num = default_node_num(vars);
    }
    if args.cache_size == 0 {
        args.cache_size = args.node_num / 2;
    }
    if args.iters == 0 {
        args.iters = 1;
    }
    if args.threads == 0 {
        args.threads = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
    }

    println!(
        "n={} vars={} init_node_num={} init_cache_size={} iters={} threads={}",
        args.n, vars, args.node_num, args.cache_size, args.iters, args.threads
    );

    for iter in 0..args.iters {
        let manager: DefaultManager = DefaultManager::init(args.node_num, args.cache_size, vars);
        let counter = AtomicOpCounts::default();

        let start = Instant::now();
        let result = build_n_queens_mt(&manager, args.n, args.threads, &counter);
        let elapsed = start.elapsed();
        let ops = counter.snapshot();
        let ops_total = ops.and + ops.or + ops.comp;

        std::hint::black_box(result.root.id);
        if result.root.refed {
            manager.deref_bdd(result.root.id);
        }

        println!(
            "iter {}: time_ms={} constraints={} ops_and={} ops_or={} ops_comp={} ops_total={}",
            iter + 1,
            elapsed.as_millis(),
            result.constraints,
            ops.and,
            ops.or,
            ops.comp,
            ops_total
        );
    }
}

struct Args {
    n: usize,
    node_num: usize,
    cache_size: usize,
    iters: usize,
    threads: usize,
    help: bool,
}

impl Args {
    fn from_env() -> Self {
        let mut args = std::env::args().skip(1);
        let mut out = Args {
            n: 8,
            node_num: 0,
            cache_size: 0,
            iters: 1,
            threads: 0,
            help: false,
        };

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "--n" => out.n = parse_usize(&arg, args.next()),
                "--node-num" => out.node_num = parse_usize(&arg, args.next()),
                "--cache-size" => out.cache_size = parse_usize(&arg, args.next()),
                "--iters" => out.iters = parse_usize(&arg, args.next()),
                "--threads" => out.threads = parse_usize(&arg, args.next()),
                _ => {
                    eprintln!("Unknown arg: {arg}");
                    out.help = true;
                    break;
                }
            }
        }
        out
    }
}

fn parse_usize(flag: &str, value: Option<String>) -> usize {
    value
        .as_deref()
        .unwrap_or_else(|| missing_value(flag))
        .parse::<usize>()
        .unwrap_or_else(|_| invalid_value(flag))
}

fn missing_value(flag: &str) -> &str {
    eprintln!("Missing value for {flag}");
    std::process::exit(1);
}

fn invalid_value(flag: &str) -> ! {
    eprintln!("Invalid value for {flag}");
    std::process::exit(1);
}

fn print_help() {
    println!(
        "N-Queens BDD example (multi-thread)\n\n\
Usage:\n  cargo run --example n_queens_mt -- [options]\n\n\
Options:\n  --n <N>            Board size (default: 8)\n  --node-num <N>     Initial node table size (default: auto)\n  --cache-size <N>   Cache size (default: node_num/2)\n  --iters <N>        Iterations (default: 1)\n  --threads <N>      Worker threads (default: logical CPUs)\n  -h, --help         Show help\n"
    );
}
