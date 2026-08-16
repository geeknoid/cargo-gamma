#![doc(hidden)]
#![forbid(
    unsafe_code,
    reason = "raw platform calls stay in `cargo-gamma-unsafe`; the source engine has no reason to use them"
)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(
    clippy::redundant_pub_crate,
    reason = "this non-published crate exposes implementation types only so workspace crates can compose the engine"
)]

//! The reusable Rust source pipeline behind cargo-gamma.
//!
//! This crate is an internal workspace boundary, not a supported external API. It owns parsing,
//! cfg evaluation, operator selection, candidate collection, stable source identity, source-level
//! interning, and schema instrumentation. Cargo planning, run policy, suppression, reporting,
//! execution, and process supervision remain outside it.

use rustc_hash::{FxHashMap, FxHashSet};

pub(crate) type HashMap<K, V> = FxHashMap<K, V>;
pub(crate) type HashSet<V> = FxHashSet<V>;

/// The engine's error result.
pub type Result<T, E = Error> = core::result::Result<T, E>;

pub mod cfg;
mod error;
pub mod model;
pub mod ops;
pub mod parse;
pub mod schema;

pub use error::Error;
