#![doc(hidden)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! This is an implementation detail of the cargo-gamma tool. Do not take a dependency on this
//! crate as it may change in incompatible ways without warning.
//
// Every raw platform call cargo-gamma makes, behind an interface that is safe to call.
//
// # Why this crate exists
//
// Two things the tool has to do have no safe expression in `std`.
//
// **Killing a whole subtree.** A test that shells out to a server, a database or another build
// leaves those behind when the harness above them is cut off. `std` can start a child and kill
// *that* child; it has no way to say "and everything descended from it". Both platforms do — a
// process group on Unix and a job object on Windows — and neither is reachable except through the
// C or Win32 interface.
//
// **Bounding what a subtree allocates.** The same boundary is the only place that can account for
// the whole tree rather than the one process at its root, which on Linux means a cgroup leaf and
// the `pre_exec` hook that puts the child in it before it runs.
//
// Neither is a case of reaching for `unsafe` to go faster. There is no safe version to prefer.
//
// # What this crate promises
//
// **Nothing here is `unsafe` to call.** Every entry point is a safe function, and every obligation
// the platform imposes is discharged inside this crate rather than passed to the caller — with no
// exception, since the one obligation that could not be discharged here, mutating the process
// environment on multithreaded Unix, is not offered at all: a value that has to reach a child
// belongs on that child's `Command`, not on this process.
//
// Concentrating it here is what lets every other crate in the workspace carry
// `#![forbid(unsafe_code)]`, which turns "we reviewed the unsafe code" into a property the compiler
// checks on every build. `cargo-gamma-rt` is the one exception, and only because it is injected
// into the dependency graph of the crate under test and so cannot depend on this crate — or on
// anything else.
//
// # What belongs here
//
// A raw platform call, and the smallest amount of logic needed to make it safe to expose. The
// bounded process lifecycle that composes these calls lives in `cargo-gamma-process`; policy does
// not belong in either crate. What a memory ceiling should be is arithmetic on a baseline
// measurement, and it stays in `cargo-gamma-lib` where it can be tested without a kernel.

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(unix)]
pub mod group;
#[cfg(unix)]
pub mod interrupt;
#[cfg(windows)]
pub mod job;

mod support;

pub use support::support;
