# cargo-gamma-rt

Runtime support for [`cargo-gamma`](https://github.com/geeknoid/cargo-gamma).

This crate is injected into the dependency graph of the crate under test while a mutation
run is in progress. You should never need to depend on it directly.

It has zero dependencies, no features, no build script and no `std`, by design: anything else
would perturb feature unification in the tree under test, or stop a `no_std` tree from building
at all once the shim is injected into it.
