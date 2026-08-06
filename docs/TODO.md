# TODO

Work that is known to be missing, with enough context to pick it up cold.

## Make the Tokio smoke test complete observably

A full default-profile run against `https://github.com/tokio-rs/tokio` produced no output for more
than an hour on Windows and was stopped before cargo-gamma reported whether it had finished the
instrumented build or reached the baseline. Reproduce this with visible progress and enough
phase-level diagnostics to distinguish discovery, convergence, test-binary build, and baseline
execution. The smoke test should finish within a documented budget, or fail with a diagnostic that
identifies the phase and operation consuming that budget.

## Assess onboarding Oxidizer crates

Review the crates in `https://github.com/microsoft/oxidizer` for suitability as cargo-gamma users.
Identify crates with stable, reasonably bounded test suites, run cargo-gamma against representative
ones, document compatibility or performance blockers, and determine what configuration or code
changes would be needed to onboard them. Oxidizer is a sister project, so regressions found there
should become durable cargo-gamma tests where practical.

Note that this item concerns Oxidizer as a *consumer* of cargo-gamma. The opposite direction —
cargo-gamma taking a dependency on an Oxidizer crate — was assessed against the `fa2de0c` tree and
declined; do not re-open it without new evidence. Most of the fifty crates are async service
infrastructure and cannot apply to a synchronous CLI that shells out to cargo. The four that
warranted a real look each failed for a specific reason:

- **`tick`** was the closest fit, and `SimpleClock::new_system` needs no async runtime, but
  cargo-gamma's deadlines are poll loops waiting on a live cargo or test child. A virtual clock
  cannot advance a real build, so mocking time buys nothing, and the whole suite sleeps for about
  170 ms in total anyway.
- **`ohno`** optimizes for backtraces, enrichment stacking and typed hierarchies, which are
  log-aggregator concerns. `error.rs` is deliberately built around the usage-versus-runtime
  distinction the exit-code scheme depends on, and writes for the person who ran the command.
- **`internity`** targets millions of strings. The string-keyed maps in `discover` are
  package-reachability graphs holding at most a few hundred workspace package names.
- **`multitude`** and **`plurality`** address allocation, which is not where the time goes; the run
  is dominated by spawning cargo and executing test suites, and mimalloc is already in use.

Three constraints apply to any future candidate regardless of its merits. Every Oxidizer crate
declares `rust-version = "1.93"` against this workspace's `1.90`, and a tool people `cargo install`
should not force a three-release jump for convenience. All of them are pre-1.0, so their types
would leak into the published `cargo-gamma-lib` API and pin consumers to that churn. And
`cargo-gamma-rt` is deliberately dependency-free, which nothing may compromise.

