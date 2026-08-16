# cargo-gamma-attrs-impl

The implementation behind [`cargo-gamma-attrs`](../cargo-gamma-attrs), which is where the inert
`#[gamma::skip]`, `#[gamma::expect_survived]` and `#[gamma::expect_killed]` attributes are actually
exposed.

You almost certainly want that crate instead. This one is a normal library rather than a
proc-macro crate for one reason: a proc macro's code runs inside `rustc`, while some *other* crate is
being compiled, which puts it out of reach of both a coverage harness and a mutation run. Keeping
the logic here means it is called by ordinary tests, so it can be covered and its mutants can be
killed. What remains in the proc-macro crate is a shim thin enough to read at a glance.

## Stability

This crate is an implementation detail of `cargo-gamma-attrs` and carries no stability guarantee of
its own. Depend on `cargo-gamma-attrs`.
