#![doc(hidden)]
#![forbid(
    unsafe_code,
    reason = "raw platform calls stay in `cargo-gamma-unsafe`; this crate only composes its safe interfaces"
)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! Killing a test binary and everything it spawned, and accounting for what it used.
//!
//! Killing the process a run started is not enough. A test that shells out to a server, a database
//! or another build leaves those behind when the harness above them is cut off, and they take two
//! things with them: file locks inside the scratch tree, which turn the next run into a failure
//! that has nothing to do with any mutant, and inherited pipe handles, which keep whoever is
//! reading this tool's output from ever seeing end of file. A run that ends with a hung consumer is
//! worse than one that ends with a wrong verdict, because nobody can even see the verdict.
//!
//! Both platforms have a way to say "this process and everything descended from it" — a process
//! group on Unix and a job object on Windows — but neither is reachable from `std`. The raw calls
//! live in `cargo-gamma-unsafe`, which exposes safe interfaces; this crate composes them into the
//! lifecycle used by the rest of the tool.
//!
//! The same boundary accounts for memory because it is the only place that knows the whole process
//! tree rather than only its leader. A [`MemoryRequest`] passed to [`prepare`] asks for measurement,
//! a ceiling, or neither; [`ProcessTree::usage`] answers once the process tree is gone. On Windows
//! the job object that kills the tree also carries the limit and accounting; on Linux a cgroup leaf
//! supplies both — see [`cargo_gamma_unsafe::cgroup`].
//!
//! The boundary is in force from the child's first instruction. On Linux the child moves itself
//! into the cgroup between fork and exec; on Windows it starts suspended, enters the job, and only
//! then runs. A peak that reached the limit is therefore enforced by the kernel rather than
//! inferred after the fact.
//!
//! A terminal delivers `Ctrl-C` to the whole foreground process group, so a child sharing this
//! process's group dies with it automatically while a child leading its own group does not. Windows
//! preserves that guarantee through a job that dies with its last handle. Unix installs explicit
//! interruption handling — see [`cargo_gamma_unsafe::interrupt`].

mod memory_request;
mod memory_usage;
mod process_tree;

#[cfg(any(test, feature = "fault-injection"))]
pub mod faults;
#[cfg(test)]
mod testing;

pub use cargo_gamma_unsafe::support;
pub use memory_request::MemoryRequest;
pub use memory_usage::MemoryUsage;
pub use process_tree::{ProcessTree, SpawnGuard, capacity, prepare};
