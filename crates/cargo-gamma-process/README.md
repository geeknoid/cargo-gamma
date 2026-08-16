# cargo-gamma-process

This crate is an internal implementation detail of
[`cargo-gamma`](https://crates.io/crates/cargo-gamma). It contains cargo-gamma's bounded
process-tree containment, accounting, observation, and termination lifecycle.

Do not depend on it directly. Its API may change incompatibly without notice; it is published only
so that `cargo-gamma` can be installed through crates.io.
