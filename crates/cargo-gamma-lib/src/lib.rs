#![doc(hidden)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! This is an implementation detail of the cargo-gamma tool. Do not take a dependency on this
//! crate as it may change in incompatible ways without warning.
//
// Everything cargo-gamma does, apart from talking to the real terminal.
//
// The `cargo-gamma` binary is a few dozen lines that implement `Host` and call `run`. Code in a
// `[[bin]]` target cannot be linked by an integration test, so putting anything here that could
// live in a library would be putting it somewhere no test can reach — an unusually bad trade for a
// tool whose subject is test quality.
//
// # What the tool does
//
// Conventional mutation testing rebuilds the crate under test once per mutant. A workspace with ten
// thousand mutants and a ninety-second build spends ten days building and a few minutes testing.
//
// cargo-gamma builds **once**. Every selected mutant is compiled into the same set of test binaries
// as a *guard* — a branch, taken only when that mutant's ordinal matches the one named by the
// `GAMMA_ACTIVE` environment variable:
//
//     original:     a < b
//     instrumented: (if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })
//
// This is the *mutant schema*: one artifact encoding the whole population, with the choice deferred
// from compile time to process start. Testing a mutant then costs one process launch instead of one
// build, and the guard itself costs a cached atomic load and a branch the CPU learns immediately.
//
// # The pipeline
//
// A run moves through these stages, in order. Each module names one of them.
//
// | Stage | Module | What it produces |
// |---|---|---|
// | Command line | `commands` | The parsed request, folded together with the config file |
// | Configuration | `config` | `.cargo/gamma.toml`, with precedence against the command line decided in one place |
// | Enumeration | `discover` | Workspace packages, source files, the shard slice, and which package can reach which |
// | Parsing | `parse` | An AST with byte-accurate spans, plus the comment trivia suppression needs |
// | Mutation | `ops` | Candidate mutants from the operator registry — the catalog of what can be changed |
// | Suppression | `suppress` | The mutants withdrawn by an attribute, a comment directive, or a config rule |
// | Identity | `model` | Content-addressed mutant IDs, outcomes, and the score they roll up into |
// | Instrumentation | `schema` | The rewritten sources, the guard for each mutant, and the rollback loop that withdraws whatever will not compile |
// | Execution | `exec` | The scratch tree with the guard runtime vendored into it, one build, a measured baseline, then every mutant run in parallel under a timeout and a stall detector |
// | Projection | `report`, `elements`, `html`, `ci` | Console output, the `mutation-testing-elements` document, a self-contained page, and SARIF plus CI annotations |
//
// The `vendor` directory beside these modules is not one: it holds the report viewer and the report
// schema, embedded so that an HTML report opens on a machine with no network at all.
//
// The rest stand beside the pipeline rather than inside it:
//
// - `estimate`: stops a run at the point it would stop measuring and start waiting, and projects
//   the rest — so a four-hour job is discovered in the first minute rather than the last.
// - `advise`: turns a finished run into findings, each with a measured symptom, a remedy, and what
//   the remedy costs in signal. This is what `--advice` writes and what the CI job summary carries.
// - `fix`: plans and applies the source edits behind the `suppress` command.
// - `merge`: combines per-shard reports into one score, so a nightly job covering a slice at a time
//   still adds up to an answer about the whole workspace.
// - `migrate`: translates a cargo-mutants project into this one's vocabulary.
// - `bounds`: the timeout arithmetic — baseline, multiplier, floor — kept in one place so that
//   every command sizes a budget the same way.
// - `diag`: the hidden `--diag` dump, which reports where a run's wall clock actually went. It
//   exists for developing this tool, not for using it.
// - `error`: the error type, its cause chain, and the usage-versus-failure distinction that picks
//   the exit code.
//
// # Conventions
//
// - Every fallible path returns [`Result`], whose error carries a cause chain and knows whether it
//   is a usage error, because that distinction is what picks the process exit code.
// - Nothing writes to `stdout` or `stderr` directly; everything goes through [`Host`], which is
//   what makes the console UI, the color decisions and the exit codes ordinary assertions in a
//   test rather than things verified by eye.
// - Hash maps are `rustc_hash`, not the standard library's. None of the keys here are attacker-
//   controlled, and the cost of a DoS-resistant hash on several hundred thousand mutant IDs is not.

/// The result type used throughout the crate.
///
/// The error carries a cause chain and knows whether it is a usage error, which is what decides
/// the process exit code.
pub type Result<T, E = error::Error> = core::result::Result<T, E>;

use rustc_hash::{FxHashMap, FxHashSet};

pub(crate) type HashMap<K, V> = FxHashMap<K, V>;
pub(crate) type HashSet<V> = FxHashSet<V>;

/// Declares modules that are public to the crate's own tests and private otherwise.
///
/// Integration tests reach directly into the internals and test them at the level they are designed
/// at, with no `pub(crate)` escape hatches and no `#[cfg(test)]` re-exports widening the API
/// surface. The `internals` feature is enabled by this crate's own dev-dependency on itself, so it
/// is on for test builds and off for everything else — which keeps the real privacy boundary, and
/// the dead-code analysis that depends on it, for ordinary builds.
///
/// It is gated on a feature rather than on `debug_assertions` because `cargo test --release` turns
/// `debug_assertions` off while still building the integration tests, which then fail to compile.
macro_rules! declare_modules {
    ($($name:ident),+ $(,)?) => {
        $(
            #[cfg(feature = "internals")]
            #[doc(hidden)]
            pub mod $name;
            #[cfg(not(feature = "internals"))]
            mod $name;
        )+
    };
}

declare_modules!(
    advise, bounds, cfg, ci, commands, config, diag, discover, docs, elements, error, estimate, exec, fix, html, merge, migrate, model,
    ops, parse, report, schema, suppress
);

#[cfg(test)]
pub(crate) mod testing;

pub use crate::commands::{Host, run};
