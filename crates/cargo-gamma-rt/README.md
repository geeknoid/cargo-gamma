# cargo-gamma-rt

Runtime support for [`cargo-gamma`](https://github.com/geeknoid/cargo-gamma).

This crate is injected into the dependency graph of the crate under test while a mutation
run is in progress. You should never need to depend on it directly.

It has zero dependencies, no features and no build script, by design: anything else would
perturb feature unification in the tree under test.
