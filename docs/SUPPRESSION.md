# Suppressing mutations

Not every surviving mutant is a missing test. A mutant can be *equivalent* — a program that behaves
identically to the original, so no test could ever tell them apart — or it can sit in code that is
deliberately untested. Both waste a reviewer's attention every run until somebody records the
decision, and a suppression is that record.

* [Choosing a channel](#choosing-a-channel)
* [In-source directives](#in-source-directives)
* [Where a directive can go](#where-a-directive-can-go)
* [Recording expectations instead of hiding mutants](#recording-expectations-instead-of-hiding-mutants)
* [Suppressing in bulk](#suppressing-in-bulk)
* [cargo-mutants attributes](#cargo-mutants-attributes)

## Choosing a channel

There are five ways to stop a mutant being counted against you. They differ in where the decision
lives and in how visible it is to the next person to read the code.

| Channel | Scope | Where it lives | Reach for it when |
| --- | --- | --- | --- |
| `#[gamma::skip]` | one item, block, statement or expression | beside the code | one specific site is equivalent or deliberately untested |
| `// gamma::skip` | the same, without an attribute | beside the code | the site cannot carry an attribute, or you would rather not add one to shipped source |
| `--ops !family` | the whole run | the command line or `gamma.toml` | a whole class of mutation does not apply to this project |
| `--exclude-file` | every mutant in matching files | the command line or `gamma.toml` | generated code, vendored code, or a module you are not responsible for |
| `--exclude-test` | the tests that run, not the mutants | the command line or `gamma.toml` | a slow or flaky test should not be part of the kill decision |

The first two are almost always the right answer. A directive next to the code says *this* mutant is
equivalent and says why; a config-level exclusion says nothing about any individual site and quietly
grows to cover code written years later that nobody ever considered.

`--exclude-test` is the odd one out: it does not suppress a mutant at all, it narrows the test suite
each mutant is run against. Excluding a test makes mutants *harder* to kill, not easier.

## In-source directives

A directive is one of three names in the `gamma` namespace:

```rust
#[gamma::skip]           // do not generate mutants here
#[gamma::expect_missed]  // mutants here are expected to survive
#[gamma::expect_caught]  // mutants here are expected to be killed
```

Anything else in the `gamma::` namespace is a usage error rather than something quietly ignored, so
a typo in a directive name is reported instead of silently disabling nothing. Attributes in other
namespaces are left alone.

### Selecting what to suppress

A bare directive covers everything:

```rust
#[gamma::skip]
fn hash_seed() -> u64 { 0x9E37_79B9_7F4A_7C15 }
```

Arguments narrow it, using exactly the same grammar as `--ops` — mutator names, family prefixes,
`@profiles` and `!` negation, comma separated:

```rust
#[gamma::skip(relational)]                  // one family
#[gamma::skip(relational.lt_to_le)]         // one mutator
#[gamma::skip(@arithmetic)]                 // a profile
#[gamma::skip(arith, literal.int)]          // several selectors
#[gamma::skip(@all, !fn_value)]             // everything but one family
```

Sharing the grammar is deliberate. A selector you worked out at the command line while narrowing
down a result pastes straight into the source once you decide the answer is permanent.

### Recording why

Two named arguments may follow the selectors in any position:

```rust
#[gamma::skip(literal, reason = "the seed is arbitrary; any value works", tag = "equivalent")]
fn hash_seed() -> u64 { 0x9E37_79B9_7F4A_7C15 }
```

`reason` is free text and appears in the report and in `cargo gamma explain`, so the next person to
wonder about the gap gets the answer without a `git blame`. `tag` is a short label you choose, which
lets a report be grouped by category — `equivalent`, `perf`, `unsafe` — and lets a review ask how
many suppressions of a given kind a change added.

Both are optional, but a `skip` with no `reason` is a decision nobody can audit.

### The comment form

Every directive has a comment spelling, which is character-for-character the attribute with `//` in
front:

```rust
// gamma::skip(literal, reason = "the seed is arbitrary")
fn hash_seed() -> u64 { 0x9E37_79B9_7F4A_7C15 }
```

It exists for the places an attribute cannot go, and for codebases that would rather not carry a
tool's attributes in shipped source. It behaves identically otherwise.

## Where a directive can go

An attribute may be placed on any of:

* a free function (`fn`)
* an inherent or trait method (`impl` / `trait` body)
* an `impl` block, covering everything in it
* a `mod`, covering everything in it
* a statement
* an expression

A **comment on its own line** governs the outermost construct that begins after it. A **trailing
comment** — one following code on the same line — governs the widest span that starts on that line.
So this suppresses the whole `if`, not just the condition:

```rust
if a < b && c > d {   // gamma::skip(relational)
```

while this suppresses only the statement that follows:

```rust
// gamma::skip(stmt)
counter += 1;
```

Placing a directive on a `mod` or an `impl` is the bluntest form: it will keep covering code added to
that block long after the reason it was written stops applying. Prefer the narrowest placement that
covers the site.

## Recording expectations instead of hiding mutants

`skip` stops a mutant being generated. `expect_missed` and `expect_caught` still generate it, still
run it, and turn the result into an assertion:

```rust
#[gamma::expect_missed(reason = "logging only; nothing observes this")]
fn trace(&self) { ... }
```

An `expect_missed` mutant that gets killed is reported, and so is an `expect_caught` mutant that
survives. That makes the annotation self-correcting: when somebody finally writes the test that
covers this code, the run tells you the note is stale instead of leaving it to rot.

Use `expect_missed` where `skip` is tempting but the gap is a real gap you intend to close, and
`expect_caught` to pin coverage you consider load-bearing so that a later change cannot quietly
remove it.

## Suppressing in bulk

After a run, to record the mutants it could not decide on:

```bash
cargo gamma suppress
```

This performs a run and then writes directives into the source at each eligible site, giving a
baseline that a later run can be compared against: anything new is a regression introduced by the
change under review. `--dry-run-suppress` prints the diff without touching anything, and
`--eligible` chooses which verdicts qualify — it defaults to `timeout`.

**A survivor is never eligible, and cannot be made eligible.** A surviving mutant is a real gap in
the test suite, and suppressing it in bulk would remove the gap from the score rather than from the
code. Deciding a particular survivor is equivalent is a judgement about one site, so it takes a
directive written at that site, with a `reason`.

Review the diff before committing it. A bulk suppression is a snapshot of what one run could not
resolve on one day, not a judgement that any of it should stay that way.

To see what a mutator does and how to switch it off:

```bash
cargo gamma explain relational.lt_to_le   # a mutator
cargo gamma explain @arithmetic           # everything a profile selects
```

## cargo-mutants attributes

`#[mutants::skip]` is honoured as though it were `#[gamma::skip]`, so a codebase already annotated
for cargo-mutants needs no changes to get the same exclusions here. The `// mutants::skip` comment
form and the `#[cfg_attr(test, mutants::skip)]` spelling are both recognised too.

Configuration is the one thing not read across: `cargo gamma migrate` translates
`.cargo/mutants.toml` into `.cargo/gamma.toml` rather than the two tools sharing a file. The
catalogs differ, so honouring another tool's settings silently would mean them quietly changing
which mutants this one skips, and therefore quietly changing the score. Nothing is dropped in
translation — a key that cannot be expressed becomes a `TODO` carrying the original text.

```bash
cargo gamma migrate --dry-run   # see the translation without writing it
```
