# tobdd

In the src folder where the rust code lives:

- When using format! and you can inline variables into {}, always do that.
- Use `debug_assert!` instead of `assert!` to avoid performance loss in release
  profile.
- Install any commands the repo relies on (for example `rg`, or `cargo-insta`)
  if they aren't already available before running instructions here.

Run `cargo fmt --all` automatically after making Rust code changes; do not ask
for approval to run it. Additionally, run the tests:

1. Run the test for the specific project that was changed. For example, if
changes were made in `src/node`, run `cargo test -p node`.
2. Once those pass, if any changes were made in common, core, or protocol, run
the complete test suite with `cargo test --all-features`.
