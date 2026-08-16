//! Surgical, in-source control over which mutants are generated.
//!
//! A mutation tool that cannot be told "not here, and here is why" gets switched off. Some
//! surviving mutants are genuinely uninteresting — a debug formatter, a fallback whose two arms
//! are observationally identical, a hot loop bound that no test should be asked to pin down — and
//! if the only way to silence them is a global flag, the useful signal goes with them.
//!
//! # Three channels, one vocabulary
//!
//! A directive can arrive as a real attribute, as a comment, or from configuration. All three name
//! mutators with the same selector language used by `--mutators`, so there is exactly one thing to
//! learn.
//!
//! ```text
//! #[gamma::skip(arith, reason = "fixed-point math, covered by proptest")]
//! fn scale(a: i64, b: i64) -> i64 { a * b / 1000 }
//! ```
//!
//! # Why comments look exactly like attributes
//!
//! Attributes on statements and expressions are still unstable in Rust, so an attribute cannot be
//! placed on the one line a user actually wants to exempt. The comment form is deliberately the
//! attribute with `//` in front of it, and its body is handed to the same attribute parser so the
//! two forms cannot drift apart:
//!
//! ```text
//! // #[gamma::skip(arith)]
//! let total = base * rate + offset;
//! ```
//!
//! The surrounding `#[` and `]` are optional in a comment, so `// gamma::skip(arith)` says exactly
//! the same thing. Either way, a comment that opens with `gamma::` and then fails to resolve is a
//! usage error: a misspelling that was quietly dropped would read as a working suppression and
//! hand back survivors.

mod apply;
mod directive;
mod idle;
mod intent;
mod scan;
mod scopes;

pub use apply::suppress;
pub use directive::Directive;
pub use idle::{Idle, idle};
pub use intent::Intent;
pub use scan::directives;

#[cfg(test)]
mod tests;
