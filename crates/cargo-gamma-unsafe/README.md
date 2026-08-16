# cargo-gamma-unsafe

The platform calls [`cargo-gamma`](https://github.com/geeknoid/cargo-gamma) cannot make safely,
behind an interface that is safe to call. This crate is an implementation detail of the tool; you
should never need to depend on it directly.

Two things the tool does have no safe expression in `std`: killing a whole process subtree (a
process group on Unix, a job object on Windows) and bounding what that subtree allocates (a cgroup
leaf on Linux, the same job object on Windows). Neither is a case of reaching for `unsafe` to go
faster — there is no safe version to prefer.

Concentrating those calls here is what lets every other crate in the workspace carry
`#![forbid(unsafe_code)]`, which turns "we reviewed the unsafe code" into a property the compiler
checks on every build. `cargo-gamma-rt` is the one exception, and only because it is injected into
the dependency graph of the crate under test and so can depend on nothing at all.

Policy does not live here. What a memory ceiling *should* be is arithmetic on a baseline
measurement, and it stays in `cargo-gamma-lib` where it can be tested without a kernel. This crate
answers "what can the platform do, and do it"; its caller answers "what should we ask for".
