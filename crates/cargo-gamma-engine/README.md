# cargo-gamma-engine

This crate is an internal implementation detail of
[`cargo-gamma`](https://crates.io/crates/cargo-gamma). It contains the Rust source parsing,
mutation collection, stable identity, and schema instrumentation pipeline.

Do not depend on it directly. Its API may change incompatibly without notice; it is published only
so that `cargo-gamma` can be installed through crates.io.
