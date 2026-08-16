#![forbid(
    unsafe_code,
    reason = "every raw platform call in this workspace lives in `cargo-gamma-unsafe`, behind a safe interface"
)]

//! The logic behind the inert attribute macros that `cargo-gamma-attrs` exposes.
//!
//! # Why this crate exists
//!
//! `cargo-gamma-attrs` is a proc-macro crate, and a proc macro's code runs only inside `rustc`,
//! while some *other* crate is being compiled. That puts it beyond the reach of both measurements
//! this project cares about:
//!
//! - A coverage harness collects counters from test binaries. A proc macro increments its counters
//!   inside the compiler, which writes no profile the harness sees.
//! - A mutation run selects one mutant per test process at run time. A proc macro has already
//!   finished by then, so none of its mutants can be active while a test is watching.
//!
//! Splitting the logic into an ordinary library makes it reachable by coverage and mutation tests.
//! The proc-macro crate remains a thin shim.
//!
//! # What the macros accept
//!
//! See the [`cargo-gamma-attrs`](https://docs.rs/cargo-gamma-attrs) documentation for the
//! user-facing description. In brief: a comma-separated selector list, optionally followed by
//! `reason = "..."` and `tag = "..."`, both of which must be string literals.
//!
//! `#[gamma::value(<expr>)]` instead takes an expression. It is checked by [`value`], because its
//! argument is spliced into the user's crate as a mutant and must be exactly one expression.

mod implementation;

pub use implementation::{CHAIN_FACTOR, MOST_FACTOR, NESTING_LIMIT, inert, inert_timeout, value};
