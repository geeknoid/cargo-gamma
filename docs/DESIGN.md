# How cargo-gamma works

This document explains the machinery: what the tool is made of, what it does to your code, why that
makes it fast, and what the design costs. It is an architectural account of the major capabilities
and the reasoning behind them, not a tour of the source tree — a reader should come away
understanding why the numbers come out the way they do. The user-facing reference — every flag,
every mutator, every configuration key — is in [the README](../README.md); this file is about
structure and rationale.

## Contents

- [The problem](#the-problem)
- [The idea: one build, every mutant](#the-idea-one-build-every-mutant)
- [The crates](#the-crates)
- [The pipeline](#the-pipeline)
- [Encoding a mutant](#encoding-a-mutant)
- [A mutant is selected by the environment, so the environment has to hold still](#a-mutant-is-selected-by-the-environment-so-the-environment-has-to-hold-still)
- [Making it compile: the rollback loop](#making-it-compile-the-rollback-loop)
- [The scratch tree](#the-scratch-tree)
- [Running the population](#running-the-population)
- [Where the test fixtures live](#where-the-test-fixtures-live)
- [Proving the concurrency, not arguing it](#proving-the-concurrency-not-arguing-it)
- [Flaky tests](#flaky-tests)
- [Choosing a test harness](#choosing-a-test-harness)
- [Bounding what a mutant allocates](#bounding-what-a-mutant-allocates)
- [Not running what cannot matter](#not-running-what-cannot-matter)
- [Code the build never compiled](#code-the-build-never-compiled)
- [Stopping at the test that caught the mutant](#stopping-at-the-test-that-caught-the-mutant)
- [The census: reusing the guard as a coverage probe](#the-census-reusing-the-guard-as-a-coverage-probe)
- [Detecting a hang without instrumenting for it](#detecting-a-hang-without-instrumenting-for-it)
- [Where the time actually goes](#where-the-time-actually-goes)
- [Choosing what to mutate](#choosing-what-to-mutate)
- [Suppression, and the directives that assert instead of hide](#suppression-and-the-directives-that-assert-instead-of-hide)
- [Naming a mutant](#naming-a-mutant)
- [Scoping to a change, and resuming](#scoping-to-a-change-and-resuming)
- [What a run remembers](#what-a-run-remembers)
- [Sharding across nights](#sharding-across-nights)
- [Merging what the shards found](#merging-what-the-shards-found)
- [What a verdict means](#what-a-verdict-means)
- [Showing that a long build is moving](#showing-that-a-long-build-is-moving)
- [Projecting a run: the reporting surfaces](#projecting-a-run-the-reporting-surfaces)
- [Arriving from another mutation tester](#arriving-from-another-mutation-tester)
- [What this design costs](#what-this-design-costs)

## The problem

Mutation testing answers the question coverage cannot. Coverage says a line executed. Mutation
testing asks whether anything would have *complained* if that line were wrong — by changing it in a
small, deliberate way and checking that the suite fails.

The technique is sixty years old and still not routine, for one reason: cost. The obvious
implementation edits the source, compiles, runs the suite, and reverts, once per mutant. That puts a
full compile inside the inner loop:

```
total ≈ mutants × (build + suite)
```

Take a workspace with 10 000 mutants, a 90-second incremental build and a 5-second suite. That is
950 000 seconds — **eleven days** — and 95% of it is the compiler. The suite, the part that actually
produces the answer, is the remaining twentieth.

Every fast mutation tester is, at bottom, an attack on that multiplication.

## The idea: one build, every mutant

`cargo-gamma` moves the build out of the loop entirely. Every selected mutant is compiled into the
*same* set of test binaries, each behind a runtime guard, and one environment variable picks which
guard fires. The construction is called a **mutant schema**, after Untch, Offutt and Harrold, who
introduced it for Fortran in 1993.

```
total ≈ build + mutants × suite
```

Same workspace: one build — somewhat longer than 90 seconds, because the instrumented tree is
bigger — plus 10 000 suite runs. Call it 50 000 seconds, or **under 14 hours**, against eleven days.
The compile term stopped being multiplied.

The ratio is not a constant. It is roughly `(build + suite) / suite`, so the win grows with how
expensive your build is relative to your suite — which is to say, it is largest exactly where the
conventional approach is most unusable. It also shrinks toward nothing on a crate that builds in two
seconds, where there was never much to save.

What remains after the build is removed is irreducible: you cannot know whether a test catches a
mutant without running the test. The rest of this document is mostly about shaving that term
without changing any answer.

## The crates

The workspace is eight crates, and each boundary exists for a reason that is not organizational.

| Crate | Role | Why it is separate |
|---|---|---|
| `cargo-gamma` | The binary. A few dozen lines that implement `Host` and call `run`. | Code in a `[[bin]]` target cannot be linked by an integration test. Putting logic there would put it where no test can reach — a bad trade for a tool whose subject is test quality. |
| `cargo-gamma-lib` | Coordinates Cargo discovery, run policy, execution, scoring, and reporting. | `#![doc(hidden)]`, and published as an implementation detail rather than an API. The source engine and process supervisor sit behind narrower internal crate boundaries so their invariants do not depend on application policy. |
| `cargo-gamma-engine` | Parses Rust source, collects mutation candidates, assigns stable source identities, and instruments a mutant schema. | This is the reusable, deterministic source transformation at the center of the tool. It deliberately knows nothing about Cargo metadata, suppression policy, process execution, outcomes, or reports. It is published only to support the crates.io dependency chain and remains an unsupported implementation detail; the crate boundary permits later reuse without promising compatibility today. |
| `cargo-gamma-process` | Contains, observes, accounts for, and terminates one spawned process tree. | Timeouts, harness policy, and verdicts stay in `cargo-gamma-lib`; this crate owns the race-sensitive mechanism: limits active before execution, descendant-wide cleanup, interrupt-safe spawning, and observe/terminate/release/reap ordering. It is likewise published only as an unsupported implementation dependency. |
| `cargo-gamma-rt` | The guard runtime, vendored into the instrumented tree as `gamma_rt`. | It is injected into *your* dependency graph, so it carries **zero dependencies**: a dependency, a feature or a build script there could perturb feature unification in your tree, changing what your code compiles to and therefore what your tests prove. |
| `cargo-gamma-attrs` | The inert `#[gamma::skip]`, `#[gamma::test_timeout_multiplier]`, and `#[gamma]` family of attributes. | Its library name is `gamma`, so a user writes `#[gamma::skip]` with no `use` and no rename. Every macro expands to the annotated item unchanged; the attributes exist to be *seen* by this tool, not to do anything. |
| `cargo-gamma-unsafe` | Every raw platform call the tool itself makes, behind a safe interface. | Killing a whole process subtree and bounding what it allocates have no safe expression in `std`, so somewhere has to say `unsafe`. Confining those calls to one crate lets the rest of the tool carry `#![forbid(unsafe_code)]`. It exposes only safe platform primitives; `cargo-gamma-process` composes them into a lifecycle, and `cargo-gamma-lib` decides policy. |
| `cargo-gamma-attrs-impl` | The proc-macro logic behind those attributes. | A proc macro runs inside `rustc`, which puts it beyond the reach of both coverage and mutation measurement. Splitting the logic into an ordinary library brings it back within reach — the same facade / shim / implementation arrangement used elsewhere in the ecosystem. |

`Host` is the seam that keeps the console testable: nothing writes to `stdout` or `stderr` directly,
so the output, the color decisions and the exit codes are ordinary assertions in a test rather than
things verified by eye.

## The pipeline

A run moves through these stages in order. `cargo-gamma-lib` coordinates them; parsing, mutation
collection, identity, and instrumentation are implemented by `cargo-gamma-engine`, while bounded
child execution is implemented by `cargo-gamma-process`.

| Stage | Module | What it produces |
|---|---|---|
| Command line | `commands` | The parsed request, folded together with the config file |
| Configuration | `config` | `.cargo/gamma.toml`, with precedence against the command line decided in one place |
| Enumeration | `discover` | Workspace packages, source files, the shard slice, the diff slice, and which package can reach which |
| Parsing | `cargo-gamma-engine::parse` | An AST with byte-accurate spans, plus the comment trivia suppression needs |
| Mutation | `cargo-gamma-engine::ops` | Candidate mutants from the mutator registry — the catalog of what can be changed |
| Suppression | `suppress` | The mutants withdrawn by an attribute, a comment directive, or a config rule |
| Identity | `cargo-gamma-engine::model`, `model` | Content-addressed source identities, then run policy, outcomes, and the score they roll up into |
| Instrumentation | `cargo-gamma-engine::schema` | The rewritten sources and the guard for each mutant |
| Execution | `exec`, `cargo-gamma-process` | The scratch tree, one build with its rollback loop, a measured baseline, then every mutant run in parallel under a timeout, a stall detector and a memory ceiling |
| Projection | `report`, `elements`, `html`, `ci` | Console output, the `mutation-testing-elements` document, a self-contained page, and SARIF plus CI annotations |

Spans have to be byte-accurate because instrumentation is a *text splice*, not a `syn`/`quote`
round-trip: the tool rewrites the bytes it was given rather than reprinting a parsed tree, so your
formatting, macros and comments survive untouched. `parse` also scans the raw text for comment
directives, which `syn` never sees at all.

Several capabilities stand beside the pipeline rather than inside it:

- Deciding which conditionally compiled code is actually in the build (`cfg`).
- Projecting what a run will cost, from measurements rather than guesses (`estimate`).
- Turning a finished run into findings — a measured symptom, a cause, a remedy, and the cost of that
  remedy in signal (`advise`).
- Planning and applying the source edits behind the `suppress` command (`fix`).
- Combining per-shard reports into one score (`merge`).
- Translating a foreign project's settings into this one's vocabulary (`migrate`).
- Timeout arithmetic, in one place, so every command sizes a budget the same way (`bounds`).
- Rendering the mutator and profile reference tables straight from the registry, so the README
  cannot drift from the catalog (`docs`).
- The hidden `--diag` dump, which says where a run's wall clock went. It exists for developing this
  tool, not for using it (`diag`).
- The error type, its cause chain, and the usage-versus-failure distinction that picks the exit code
  (`error`).

The report viewer bundle and the report schema are vendored into the binary, so that an HTML report
opens on a machine with no network at all.

Two conventions hold throughout. Every fallible path returns a `Result` whose error carries a cause
chain and knows whether it is a *usage* error, because that distinction is what picks the process
exit code. And a run refuses anything it cannot honor rather than proceeding on a guess: an unknown
key in `.cargo/gamma.toml` stops the run and names the offender, a selector that matches no mutator
is an error with a spelling suggestion, and a test-target pattern that matches nothing is an error
too. A configuration whose settings are silently ignored is worse than none, because the project
believes it is configured.

The command surface is eight subcommands: `run`, `list`, `explain`, `migrate`, `suppress`,
`unsuppress`, `merge` and `completions`, with `run` implied when no subcommand is given. Estimation
and advice are *flags
on a run* — `--estimate` and `--advice` — rather than commands of their own, because both need the
build and the baseline that a run already pays for, and an estimate that measured differently from
the run it predicts would be worthless.

## Encoding a mutant

An expression site becomes a branch on the guard predicate:

```rust
// original
a < b

// instrumented
(if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })
```

`gamma_rt::a` is a cached atomic load and a comparison. `GAMMA_ACTIVE` is read once per process, not
once per call, so a guard costs a predictable branch that the predictor learns immediately and then
never mispredicts — 7 is either the active ordinal for the whole process or it is not.

That one read does not allocate. `std::env::var` returns a `String`, and this is the only function in
the guard's path that touches anything outside the process's own memory — it runs at whichever guard
executes first, which may be inside a block a test is measuring for allocations. So it goes to
`getenv` directly and parses out of borrowed bytes.

Rust will not accept the same text everywhere, so a guard takes one of three shapes depending on
what the site is:

| Shape | Instrumented form | Mutation |
|---|---|---|
| Expression | `(if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })` | replaces the value |
| Block | `{ if ::gamma_rt::a(12u32) { Default::default() } else { ..body.. } }` | replaces the body |
| Statement | `if !::gamma_rt::a(19u32) { self.entries.push(value); }` | deletes the statement |

The parentheses around the expression form are not cosmetic. Without them, a guard in condition
position produces `if { .. } { .. }`, which Rust rejects.

### Why the encoding stays linear

Sites nest. In `a + b < c`, the `<` site contains the `+` site. The obvious encoding instruments
both arms of every guard, which duplicates whole subtrees and grows as `2^depth` — enough to make a
deeply nested expression uncompilable.

It is also unnecessary. **Exactly one mutant is live in a process.** If the `<` mutant is active then
no `+` mutant can be, so the taken arm can hold the plain original text of its operands. Only the
`else` arm — the one reached when this site is *not* the active mutant — needs instrumented
children.

That removes the exponential blow-up, not all growth. A site's operands are still written twice —
once plain in the taken arm and once instrumented in the `else` arm — so a chain of nested sites
grows superlinearly in the depth of the nesting, roughly with the sum of the subtree sizes along it.
In real code the depth is small and the result is close to linear; the guarantee the design makes is
that it compiles, not that it is free.

The alternative, binding operands to temporaries and sharing them between the arms, was rejected. It
would defeat the short-circuit of `&&` and `||`, move values that were only borrowed, and change
when temporaries drop. Duplicating operand text costs compile time; any of those three would change
what the tests prove.

## A mutant is selected by the environment, so the environment has to hold still

The selection channel is `GAMMA_ACTIVE`, and reading it means `getenv`. That is only sound while
nothing is writing the environment, and the process doing the reading is not cargo-gamma — it is the
crate under test's own test binary, with `gamma_rt` linked into it.

cargo-gamma keeps its half of that bargain: the variable is set on the child's `Command` before the
spawn and is never touched afterwards, so the tool contributes no writer. The other half belongs to
the crate being tested, and is a precondition of using this tool:

> **A crate under mutation testing must not call `std::env::set_var` or `remove_var` while other
> threads are running.**

`cargo test` runs tests on several threads by default, so a test or fixture that sets an environment
variable races every other thread's `getenv` — including the one inside `gamma_rt::active`. In
edition 2024 that is exactly why `set_var` is `unsafe`: it is a data race in `libc` over `environ`, not
merely a confusing value. A suite that does it is already unsound on its own terms; instrumentation
adds one more reader to a process that already had several.

Two things narrow the window without closing it. The read is memoized, so it happens once per
process rather than once per guard — at whichever guard executes first. And concurrent readers are
fine with each other; only a concurrent *writer* is the hazard.

Closing it entirely would mean reading before any user thread can exist, which means a pre-`main`
constructor — an `.init_array` entry on ELF, `__mod_init_func` on Mach-O, `.CRT$XCU` on Windows.
That was considered and rejected: it puts three platforms' worth of linker-section `unsafe` into the
one crate that is compiled into other people's binaries, to defend against a pattern those binaries
should not contain, and a linker quirk in it would be a failure with no good diagnostic. Stating the
precondition is the better trade.

A suite that genuinely must mutate the environment should serialize those tests against everything
else — the usual process-wide mutex — which is what makes them sound with or without this tool.


### And the same rule turned on this codebase

The precondition above is asked of crates under test, and this crate keeps it too — more strictly,
because the suite is the hardest case there is. Forty end-to-end tests call `run` in-process, on the
harness's thread pool, while every other test in the binary is reading the environment.

So the rule here is not "take a lock before writing". It is:

> **Nothing in this workspace writes the process environment.** A value that has to reach a child
> belongs on that child's `Command`.

A lock would not have been enough. A mutex excludes only the threads that take it, and the readers
are everywhere — `env::var` inside production code that any test happens to call looks nothing like
touching shared mutable state, and is. There is no lock a writer can take that a reader in
`Workspace::cargo` has also taken.

The loader search path, the thread stack floor and the harness width all reach children this way:
each is derived once, from an ambient read taken before anything is spawned, and then set on every
launched `Command`. Deriving once and setting per command ensures that the baseline and the sweep
do not disagree about the workload they measure and judge, without mutating process-global state.

No exemption exists — not even for the tests. A handful of them need cargo to read `RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS` or `CARGO`, which cargo takes from the environment and nowhere else. Those
tests re-execute the test binary as a child with the variables set on its `Command`, so the code
under test reads the values it was *launched* with; the multithreaded parent never writes its own
environment, and the child only ever reads what it inherited. There is nothing to restore on a
panic and no reader to race.

The rule is enforced rather than merely stated: `nothing_writes_the_process_environment` in
`crates/cargo-gamma-lib/tests/docs.rs` fails on a `set_var` or `remove_var` in any file at all.
A lock is not enough when readers are everywhere and outside it; because tests re-execute a child
instead, no file is exempt.

## Making it compile: the rollback loop

Not every mutation is well-typed. Replacing a function body with `Default::default()` requires the
return type to implement `Default`. Swapping `+` for `-` on a type that implements `Add` but not
`Sub` does not compile. Conventional tools discover this one mutant at a time, and each discovery
costs a build.

Because the whole population lives in one tree here, one bad mutant would break the *entire* run. So
the build is a fixpoint loop:

1. Instrument the tree with every mutant not yet withdrawn.
2. Run `cargo build --keep-going --message-format=json`.
3. If it succeeded, stop.
4. Otherwise attribute each compiler diagnostic to a mutant, withdraw the mutants blamed, and go
   back to step 1.

`--keep-going` tells cargo to build the crates it still can after one fails, so a round collects
diagnostics from across the graph instead of stopping at the first broken crate.

That loop runs once per package, in dependency order, rather than once over the workspace. Packages
are grouped into stages by the size of their reach set — a dependency's reach set is a strict subset
of its dependent's, so ascending size is a topological order, and two packages with equal reach sets
are mutually reachable and share a stage. Each stage is scanned, instrumented, converged with
`cargo build -p` over just that stage's packages, and then left alone. A crate is therefore rebuilt
for its own unviable mutants rather than for every other crate's, and the run reports one line per
package — what it found and what it withdrew — as that package finishes, rather than parsing the
whole workspace before anything else can start and then going silent for the length of a global
convergence.

Each package is named on a `Mutating` progress line before its files are read. Once its build
finishes, that same line changes to `Mutated` and reports what survived compilation. Scanning and
compiling a large crate is the longest a run goes without saying anything, and what makes that wait
legible is knowing whose wait it is; the count has to wait for the build, because until then there
is no way to know which mutants were viable. Other open phase lines follow the same convention:
`Copying` becomes `Copied`, `Validating workspace` becomes `Validated workspace`, `Baselining`
becomes `Baseline`, and `Optimizing` becomes `Optimized` when their work finishes. The completed
baseline line contains only the measured test, duration, and memory statistics rather than
repeating the description of the work that produced them.

Optimization replaces its opening line with a live completed/total bar over test binaries. A binary
advances the bar when all of its independently censused test cases finish; a binary that cannot be
listed, contains no tests, or produces an untrustworthy census also advances it because that binary
has finished by conservatively falling back to whole-binary execution.

The baseline uses the same completed/total test-binary bar while it runs the suite. It deliberately
omits elapsed time and an ETA: binary durations vary too much for their count to support a stable
projection, while the count itself is exact and enough to show that the phase is moving.

Cargo's live `Building` bar temporarily owns the same terminal row as an active phase. The phase
and bar replace one another in one terminal write, so the row is never erased in a separate frame;
when a build finishes, the active phase is restored until its own completed result replaces it.

Building a subset of a workspace compiles it under narrower features than the real build will use,
because cargo unifies features across the packages it was asked to build. That is fatal for
enumerating test binaries — `cargo test --no-run -p exemel-core` fails on items that a dependent's
`features = ["xml11"]` would have configured in — but it is harmless here, for two reasons. Every
mutant lives in a lib or bin target, never in a test, bench or example, so a library-only build sees
every diagnostic a mutant can cause. And narrower features can only mean *less* code compiled, hence
fewer diagnostics, hence a mutant deferred to a later build rather than falsely withdrawn. The
staged builds pass `--keep-going` without `--tests` for exactly that reason.

A final convergence over the whole workspace is what settles it. It builds with real feature
unification, catches anything the staged builds could not see, and produces the test binaries, which
`cargo test --no-run` then enumerates as a separate command because `cargo test` rejects
`--keep-going`. On a sixteen-crate workspace the staged and unstaged paths withdraw the same 1975
mutants.

Attribution has to be exact, and the obvious way to do it does not work. A mutant knows the source
line it came from, but the instrumented tree does not agree with the source about line numbers: a
guard emits the mutated text *and* the original, so a site spanning several lines grows and shifts
everything below it. Guards are therefore located in the instrumented text as it is generated, and
recorded by line and column.

That still leaves the question of which of several nested guards to blame. The answer falls out of
the encoding: a guard's mutated branch is the only text in the tree that is not a copy of the
original, and because nested guards live exclusively in the `else` branch, no two mutated branches
overlap. A diagnostic landing inside one names its cause with no ambiguity — not merely the right
site, but the right mutant of that site, which matters when `0` is simultaneously mutated to `1` and
to `-1` and only the second fails to compile. Only a mutant that breaks code it merely *encloses*
needs a judgment call; there the innermost guarded site containing the diagnostic is blamed.

Each round removes every mutant blamed in that round, not one, so the loop converges in a handful of
rounds rather than one round per unviable mutant. It is capped by `--rollback-rounds`, 256 by
default, and a diagnostic that cannot be attributed to any mutant is a hard error naming the
scratch tree — that means the tool broke your code, and quietly withdrawing mutants until the
symptom disappeared would be the wrong answer.

Withdrawn mutants are counted as `unviable` rather than hidden. They are a fact about the code, and
a tool that silently drops the ones it found inconvenient is reporting a score about a population it
will not name. The count is unconditional for that reason; the list behind it is not, because a
large workspace withdraws thousands and printing them all buries the survivors that are the point.
`--unviable` asks for the list.

Each round rewrites only the files whose mutants changed. Cargo decides what to rebuild from mtime
rather than content, so writing a file back byte-for-byte would recompile its crate and everything
downstream of it. Comparing before writing avoids rebuilding unmutated crates and keeps rollback
compilation minimal.

### Reading the workspace before guessing at a value

The `fn_value` family reaches for `Default::default()` whenever it cannot name a value of a type,
and that guess was the single largest source of mutants that do not compile: 53% of this
workspace's withdrawals were `E0277`, a `Default` bound that is not satisfied.

Most of it is settled without any type resolution, because the type is usually one the workspace
itself defines and the definition is in a file that was going to be parsed anyway. So discovery
builds an index over every parsed file — which `struct`, `enum` and `union` names exist, and which
of them derive the standard `Default` or have a standard `impl Default` written for them — and a
type the index has a definition for but no `Default` is not offered the guess.

The index is evidence of *presence* and never proof of absence. A type it has no definition for
stays optimistic, because it may come from a dependency that does implement `Default`. Names are
compared unqualified, so two crates in one workspace can both define a `Config`; presence wins that
collision and neither is screened. A `Default` generated by a macro the collector never expands is
the one way the index can be wrong, and it errs in the safe direction — a mutant is withheld that
would have compiled, which costs a little signal rather than producing a wrong verdict.

Two readings sit beside it. An error type from *another* crate is treated as having no `Default`
outright, because `std::io::Error`, `anyhow::Error` and their kind do not have one and are not going
to acquire one; `core::fmt::Error` is the sole exception and is named as such. And a crate-wide
`type Result<T> = ...` alias, which is close to universal in real Rust and hides the error type from
every signature that uses it, is resolved through the same index, so `Ok(v)` becoming
`Err(Default::default())` can be screened at the call site from what the signature promised.

Building the index means every file is parsed before any mutant is emitted: one file's mutants
depend on what the others declare. A worker keeps the trees it parsed and mutates those same files
afterwards, because a `syn` tree is not `Send` — its spans carry a handle only the thread that made
them may touch — so what crosses the barrier between the phases is the index, which is only names.

### Why decrementing a zero is not screened up front

A recurring unviable mutant shape is `literal.int_decrement` turning a literal `0` into `-1` where
the surrounding type is unsigned, which rustc rejects outright as `E0600`. It looks like a candidate
for an up-front screen, but cannot be safely filtered syntactically.

Nothing in the syntax says whether a bare `0` is signed. Only 12 integer literals in the whole
population carry a suffix, so a screen that fired only on what it could prove would retire almost
none of the 105, and one that guessed would take the roughly 70 decrements that sit in signed
positions and compile perfectly well.

The other way out is the one the range family took: change the replacement rather than withhold it,
so that the mutant asks the same question in a form that always compiles. That does not work here,
and all three candidates fail for different reasons. `(0).wrapping_sub(1)` is `E0689`, because
method resolution needs a concrete numeric type and an unsuffixed literal has none yet. `0 - 1` is
rejected at const evaluation as an operation that will overflow. Wrapping the literal in
`core::num::Wrapping` reduces to the same const evaluation and fails identically.

So this mutation waits for type information, and the rollback loop absorbs it — which is exactly
the case the loop exists for.

## The scratch tree

The instrumented sources are never written where you work. The workspace is copied to
`target/gamma/tree`, and the guard runtime is vendored into it — written to disk from a copy
embedded in the tool itself, so it cannot drift from the version the guards were generated against,
and so nothing is fetched from the network. `--scratch-dir` moves the whole of `target/gamma`
elsewhere, for a workspace on a slow or full disk, or to give two concurrent runs trees of their
own.

The copy is not a naive recursive walk. It is parallel, it honors the workspace's `.gitignore`
files, and it clones files rather than reading and writing them where the filesystem can do that.
Three details there earn their complexity:

- **Ignored files are skipped**, which is usually the difference between copying a checkout and
  copying a checkout plus its build artifacts. Only the workspace's *own* ignore files count:
  reading them from parent directories outside the workspace would let a `.gitignore` you never
  see — one containing `*`, say, in a directory that happens to contain your checkout — silently
  produce an empty copy.
- **Symlinks are recreated, not followed**, so a link into a directory that no longer exists after
  the copy still fails the way it failed before, and a cycle cannot make the copy run forever.
- **Reflinks are attempted once.** If the filesystem supports copy-on-write clones the data is never
  moved at all; if the first attempt fails the tool stops asking and reads and writes for the rest
  of the run.

A copied workspace is not a working one until its manifests are repaired. A `path` dependency that
points outside the workspace root — `path = "../shared"` — resolves somewhere else entirely once the
tree has moved, so every `Cargo.toml` in the copy is rewritten to make such paths absolute against
the original location. Every manifest is visited, not only those of mutated packages, since an
unmutated package still has to build. The rewrite goes through a format-preserving TOML editor, so
your comments, key order and whitespace come through untouched.

The scratch directory is held for the run by an advisory lock on a file inside it. Two runs sharing
one would each delete the other's tree and write artifacts into one directory under two different
sets of instrumented sources, producing verdicts belonging to neither; the second run refuses
instead, and points at `--scratch-dir`. The lock lives in an open file, so the operating system
releases it however the process ends — there is never a stale lock to clear by hand.

Two more details matter for speed and for correctness:

- **Build artifacts live outside the copied tree**, in `target/gamma/build`. The scratch tree is
  rewritten at the start of every run and deleted at the end of it; the artifact directory is
  neither. Successive runs are therefore incremental, and only the crates whose source actually
  changed are rebuilt. `--leak-dirs` keeps the tree, which is how a build failure gets investigated.
- **Test binaries are launched directly, not through `cargo test`.** That skips cargo's startup and
  dependency check on every one of ten thousand launches. The cost is that the tool has to reproduce
  what cargo would have set up — in particular the dynamic loader path, without which a binary built
  against a dynamically linked `std` will not start at all. It also has to raise the stack floor,
  since a guard enlarges every frame it sits in and a deeply recursive test that fits in the default
  2 MiB unmutated may not once instrumented.

The guard runtime is vendored rather than depended on, and carries no dependencies of its own, for
the reason given in [The crates](#the-crates): it is injected into your graph, and anything it
brought with it could change what your code compiles to.

## Running the population

After the build, the run measures before it guesses:

**The baseline.** The suite runs once with no mutant active. If it is already red, everything after
it is meaningless — a failing test kills every mutant and the score comes out perfect. The baseline
also produces the two measurements everything downstream is calibrated from: how long the suite
takes, and the longest it legitimately goes silent.

**The timeout.** A mutant can turn a loop into one that never ends, so every run needs a budget.
That budget is derived per test binary from its measured baseline (1.5× by default, with a 20-second floor) rather
than fixed, because a constant is either too tight on a slow machine or useless on a fast one. A
mutant that exceeds it counts as *detected*: a hang is a behavior change the suite noticed, even
though it noticed expensively. A mutant that runs out its budget is not believed on the first try —
it is given three times the budget once more before the verdict is recorded, because a loaded
machine can starve a healthy test for longer than a tight budget allows and a false timeout is
scored as a kill.

**Parallelism.** Mutants are distributed across worker threads — one more than the available
parallelism by default — pulling from a shared queue. Each worker launches a test binary with
`GAMMA_ACTIVE` set to its mutant's ordinal. Since selection is per process and the binary is
read-only, the workers share everything and coordinate on nothing but the queue index.

**Early exit.** A mutant is killed by the *first* test that fails, so neither the rest of that
binary nor any binary after it is run — see [stopping at the test that caught the
mutant](#stopping-at-the-test-that-caught-the-mutant). Combined with the ordering below, a mutant
that any test kills is usually killed by the cheapest binary that could have killed it.

Every test process a run launches, baseline included, has `CARGO_GAMMA=1` set, so a suite that
drives cargo itself can tell that it is running inside one.

## Where the test fixtures live

Every fixture in this project is a string literal in the test that uses it, materialized into a
randomized working directory by `testing::workdir` when the test needs one on disk. There is no
`testdata` crate and no directory of checked-in sample projects, and that is deliberate.

A fixture that lives beside its test is read in the same breath as the assertion it feeds, so a
reader can tell what the test is actually claiming without opening a second file. A shared fixture
directory is the opposite: it accumulates files nobody dares change because it is never obvious which
tests depend on which byte of them, and the tests that use it end up asserting against a thing they
did not write. The blast radius of editing an inline literal is exactly one test.

The cases that look like they want a real file — source that is not plain ASCII, CRLF line endings, a
multi-crate workspace — do not actually need one. Rust string literals hold any of that directly, and
escapes make the awkward bytes *visible* at the point they matter rather than invisible in a file a
sanitizing editor will silently normalize on its next save. A workspace fixture is a handful of
`fs::write` calls under a `workdir`, which is also how the tool's own users would have built it.

The one thing this costs is that a fixture cannot be compiled by `cargo` as part of the workspace,
so a fixture that stops being valid Rust is caught by the test that builds it rather than by the
build. That is an acceptable trade for fixtures this small.

## Proving the concurrency, not arguing it

The run is concurrent in a small number of places, and two of them are pure synchronization: the
`Pulse` a reader thread and a watchdog coordinate on, and the reference-counted `Readers` gauge that
decides when the last reader is out. Both were correct by argument. Neither was demonstrated.

That distinction is not pedantry here. Every test that exercised them used real threads and real
sleeps, which samples exactly one interleaving out of the many a scheduler may choose. A mutation to
a memory ordering, a dropped `notify_all`, or a lost decrement leaves such a test green, because the
bug needs a schedule the test never forces. And the failure it produces is silent in the worst way:
a hung mutant misjudged as a timeout — which counts as a *detection* and raises the score — or a
reader thread leaked.

The usual instruments do not reach this code. Miri is disabled for a documented reason: the unsafe
here is I/O-bound and Miri cannot do I/O. Sanitizers need a supported target and still only observe
the schedules that actually occur. What is needed is not a better observer but an exhaustive one.

So `Pulse` and `Readers` live in `exec::verdict::hubs`, apart from the I/O they coordinate, and are
modeled under [`loom`](https://docs.rs/loom), which enumerates the interleavings rather than
sampling them. The models are behind `--cfg loom` and are not part of the default suite, because
loom rebuilds the crate against its own scheduler:

```
RUSTFLAGS="--cfg loom" cargo test -p cargo-gamma-lib --lib hubs::loom_models
```

Three properties are proven: a pulse wakeup is never lost under any interleaving, the reader gauge
returns to exactly zero, and the peak never understates two concurrent readers.

The reason to trust these models is that they were checked against deliberate breakage rather than
merely observed to pass. Dropping the `notify_all`, dropping the generation increment, and removing
the generation guard each produce a loom-reported deadlock; skipping the decrement and replacing the
`fetch_max` with a load-and-store each fail their assertion. A model that passes on correct code and
also passes on broken code has bought nothing, and that is the check worth re-running whenever one
of these is changed.

Two accommodations are worth knowing about, both inert in ordinary builds. Under loom `Pulse::wait`
blocks on the condvar rather than using `wait_timeout`, because loom has no clock — which is a
strengthening, not a weakness: it forces the model to rely on the generation guard and the
notification, the real mechanism, instead of being rescued by the timeout backstop. And `READERS`
is built through `loom::lazy_static!` under loom, since loom's atomics are not `const`-constructible.

## Flaky tests

A flaky test is a test that fails and then passes without the code changing. It is worse than a
failing test, because a failing test gets fixed and a flaky one gets re-run. The re-run is the
damage: once re-running is normal, every real failure gets one too, and the suite stops being
evidence of anything.

This matters more here than in most codebases. Parts of this suite coordinate threads through real
timing rather than a forced interleaving — the reader and watchdog threads in `exec/verdict.rs`
synchronize on a `Pulse` and are tested with real sleeps — and those are exactly the tests that go
intermittently red first, and exactly the ones whose intermittent redness is easiest to dismiss.

So flakes are *detected* and *surfaced*, and never absorbed:

- **Locally there are no retries.** `.config/nextest.toml` sets `retries = 0` in the default
  profile. A developer running the suite sees exactly what happened.
- **CI retries twice, and then fails anyway if the retry rescued the test.** The `ci` profile
  retries so that nextest can tell a flake from a failure — a test that fails and then passes is
  reported as `FLAKY` rather than as either — and `.github/scripts/flake-report.sh` reads the JUnit
  report, annotates each flaky test, and fails the step. A flaky run is not a green run.
- **Quarantine is explicit or it does not happen.** A test that cannot be fixed immediately gets a
  per-test override in `.config/nextest.toml`, carrying a reason and a tracking item. That puts the
  decision in a diff where a reviewer sees it. Adding retries to the default profile, loosening an
  assertion, or deleting the test are not quarantine; they are the thing quarantine exists to
  prevent.
- **The fix is a forced interleaving, not a longer sleep.** Raising a timeout makes a flake rarer
  without making it less real, and a flake that fires once a month is strictly worse than one that
  fires every run. Where the flake is genuine concurrency, the repair is a deterministic schedule.

There is one reason this cannot simply be solved with retries everywhere, and it is specific to
this tool: nextest reads `.config/nextest.toml` from the workspace *under test*, so when
`cargo-gamma` runs `cargo nextest run` against this repository, the default profile is the one in
force. A retry there could let a test that killed a mutant pass on the second attempt and record
the mutant as a survivor, which inflates the mutation score — the exact failure this tool exists to
detect. Retries stay confined to the `ci` profile, which no mutation run selects.

## Choosing a test harness

By default the test binaries are launched directly, which is what libtest is: one process running
every test in that binary, on a thread pool. `--nextest` runs them through `cargo nextest`
instead, which gives every test **its own process**.

That is not a preference about tooling. A suite that shares a mutable global, sets an environment
variable, or installs a process-wide handler is red under a threaded harness — and a red baseline
stops a run before it judges anything, because a suite that already fails kills every mutant and
returns a perfect score. Such a workspace cannot be measured at all without process isolation. The
option exists to make those trees measurable, not to make measured ones faster; process-per-test
costs more, and paying it where it is not needed is a straight loss.

The architectural difficulty is that nextest normally *builds* what it runs, which would put a cargo
invocation between every mutant and its verdict and reinstate the exact multiplication this design
exists to remove. It also accepts two metadata files describing an already-built tree, and given
those it runs binaries and calls no cargo at all. Both are produced once, immediately after the
build, and reused by every mutant for the rest of the run — so the harness changes, and the cost
model does not.

Nextest knows binaries by its own identifiers rather than by path, and the whole mapping is
established and checked once, up front. A binary nextest declines to recognize is a disagreement
about the tree rather than a fact about any mutant, and discovering it a thousand mutants into a run
would waste the build; discovering it before the first one costs nothing and names the binary.

Two mechanisms downstream read the harness's output and therefore have to know which harness
produced it: the early exit below and the stall detector after it. Each understands both formats —
libtest's `test <name> ... FAILED` and nextest's `FAIL [ 0.024s] (1/2) <binary> <name>` — so
neither capability is lost by switching.

## Bounding what a mutant allocates

A timeout is a bound on one runaway resource. Memory is the other: a mutant that removes a bound can
make a test allocate without limit, and on an unprotected machine that takes down the whole run —
along with whatever else the machine was doing — instead of producing a verdict. So allocation is
bounded on the same reasoning as wall-clock time, and by default.

The ceiling is derived rather than fixed, from the same baseline the timeout comes from: each test
binary's own peak, times a multiplier (2 by default) or plus a headroom (128 MiB by default),
whichever is larger. `--memory-limit` replaces the derivation with a stated number, and
`--memory measure` records peaks without enforcing anything.

A mutant that breaches its ceiling is `OUTOFMEM`, and that is a verdict of its own rather than a
flavor of `killed`. It counts as detected, on the timeout's reasoning — the baseline established
that this workload fits under this ceiling without the mutant, so the mutant is what changed — but a
reader who could not tell the two apart would go looking for a failing test that does not exist.

Enforcement is cgroup v2, which is the only unprivileged Linux facility that accounts for a whole
process tree as one quantity. That matters because a test can launch servers, databases and nested
cargo invocations, and `getrusage` or polling `/proc` would measure one process of several. Each
invocation gets a fresh leaf cgroup — reusing one would carry `memory.peak` across mutants and
attribute one mutant's allocation to another — and the child moves itself in from a `pre_exec` hook
that writes `"0"` to `cgroup.procs`, which is the only async-signal-safe thing available between
fork and exec. Where the controller is not already delegated, the tool first moves itself into a
supervisor subgroup so that it can delegate the controller downward.

Where the host offers none of this, what happens depends on who asked. A run that inherited the
default degrades to no enforcement and says so; a run that asked for enforcement explicitly is an
error. A tool that quietly does not protect what it said it would protect is worse than one that
does not offer the protection.

### Which containment paths are exercised, and where

The containment layer is the part of this tool with two entirely separate implementations — process
groups and cgroups on Unix, a job object on Windows — which makes it the part most able to be
correct on the machine a developer happens to use and broken on the other one. A test gated on
`cfg(unix)` does not merely leave Windows unmeasured; it leaves it *looking* measured, because the
run is green on both.

So the tests assert on files rather than on process identifiers wherever that is possible. A
grandchild that touches `started`, sleeps, and touches `finished` says exactly the same thing under
either implementation, and says a stronger thing than a pid check does: a process that was killed
after it had already finished its work was not contained in any sense that matters. The workload
itself is a small helper program compiled from a string literal, driven by directives (`sleep`,
`touch`, `spawn`, `eat`), so that nothing needs `/bin/sh`, `dd`, or `/dev/shm`.

That leaves the genuinely platform-specific claims, which are stated once each and gated honestly:
that a contained child leads its own process group, and that an interrupt reaches the group rather
than only the direct child, are Unix facts with no Windows counterpart, because the job object
subsumes both; that a contained child is inside a job, and that a child spawned without one is still
killed directly, are the Windows counterparts. Everything else — metering a subtree's peak, killing
a grandchild, the untouched-registry and slot-release bookkeeping, and both memory-ceiling verdicts
— runs on every platform the tool builds for.

The Windows job also sets `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`, and Gamma starts with the
process error mode that suppresses critical-error, fault, and file-open dialogs in its children.
Mutants are expected to drive unsafe code into native faults, and a dynamically linked test can
fail in the loader before `main`; either event must become an exit status. Without those policies
Windows opens modal UI for the test executable, holding the run behind a dialog instead of
returning control to Gamma.

What is left over is the error handling, which is the part of this layer that matters most and can
be provoked least: a cgroup removed between the spawn and the move, a job object that refuses the
process it was made for, a `waitpid` on a handle that is gone. Those are real kernel behaviors with
written responses, and none of them can be arranged from a test without root, a doctored `/sys`, or a
`Child` that is not one. So there is a small fault seam — a thread-local, one-shot request for a
named boundary to refuse — compiled only under `cfg(test)`, leaving nothing in the shipped binary but
the branch that was going to be there anyway. It is deliberately thin: it can make `contain`, the
adoption, and the wait fail, and nothing else. A seam that could make anything fail would end up
being the thing under test.

A host that cannot bound memory at all is a third case, and the tests that need it stand down rather
than pass. Standing down is announced on the standard error stream, with a running count and the
same explanation the tool itself would give a user, because a test that returns early is otherwise
indistinguishable in the output from one that ran — and these are the tests asserting that a runaway
mutant gets stopped, which is not a thing anyone should be able to believe was checked when it was
not.

## Not running what cannot matter

Three filters cut the remaining term without changing a single verdict.

**The cargo cap.** The oracle is bounded by the tests `cargo test` would run in the directory the
tool was invoked from — the package selection resolved from `--package`, `--workspace`, or the
package owning the working directory. Every other cargo command scopes itself that way, and doing
otherwise costs more than a surprise: judging a library by every package that links it makes the
crate's score a property of the workspace, so it scores well because some dependent exercises it and
a refactor in that dependent withdraws the coverage with nothing to report. It also makes the price
of a run a function of the reverse-dependency graph, which is how mutating one leaf crate ends up
compiling and testing most of the workspace.

The cap is safe to take precisely because of the uncovered verdict below: withdrawing a binary from
the oracle cannot manufacture a false survivor, only an honest report that nothing in scope tests the
code. Where the tests really do live elsewhere — a package of integration tests, a parent crate that
exercises a private implementation crate — the run detects it after the baseline, names the packages
that could have judged, and points at `--test-package` and `--test-workspace`.

**Reachability.** Within the cap, Rust cannot call code it does not link. A test binary built from a
package that does not depend — directly or transitively — on the package a mutant lives in can never
execute that mutant. Running it there costs a full suite and can only produce the answer it already
had. The dependency closure is computed from *declared* dependencies rather than a resolved graph, so
it costs nothing extra to obtain.

The filter fails open: if either package cannot be identified, it is assumed reachable, and a
package whose dependency chain cannot be graphed reaches everything. A missed skip costs a little
time; a wrong skip would hide a real gap in the test suite.

A mutant that *no* binary can reach is reported `uncovered` rather than `survived`. Both count
against the score identically, but they call for different responses — a survivor means a test ran
and did not notice, while an uncovered mutant means no test exists at all.

**A hand-chosen oracle.** Reachability is a fact about the code; which tests *should* decide a
verdict is a judgment, and it is a separate one from which code should be mutated. `--test-package`
names the packages whose tests convict, replacing the cargo cap rather than narrowing within it;
`--include-test` and `--exclude-test` work at cargo *target* granularity; and `--test-workspace` lifts
the cap entirely, so every package's tests may judge a mutant. Lifting the cap does not lift
reachability — a binary still cannot convict on code it does not link — so a mutant nothing links is
`uncovered` under `--test-workspace` too, rather than becoming a survivor no test ever exercised. The usual
reason to reach for these is a corpus that is not an oracle at all — conformance suites, fuzz seeds,
golden-file comparisons — whose failures say nothing about whether a mutant was noticed.

Choosing the oracle by hand is the one filter here that *can* change a verdict: narrowing makes
mutants harder to kill, not easier, so a run that drops targets says so. A pattern matching no declared target is an
error rather than a silent no-op, because an `--exclude-test` typo quietly widens or narrows the
oracle and reads in CI exactly like a run that went well.

**Doctests are outside the model.** `cargo test --doc` is never run, so a doctest neither kills a
mutant nor contributes to the baseline. This follows from the schema rather than sitting beside it:
the whole economy of the approach is one build and one process launch per mutant, and `rustc`
compiles and links a separate binary per doctest. Admitting them would restore a per-mutant compile
for the tests that typically assert the least. The visible effect is that coverage carried only by
doc examples shows up as `uncovered`, which makes the score a lower bound on such a crate.

**Cheapest first.** Each binary is timed during the baseline, and they are tried in ascending order
of that time. This changes no verdict, only what a verdict costs.

## Code the build never compiled

Mutants are found by reading source, and source is not the same set as what the compiler saw. A
module behind `#[cfg(feature = "serde")]` in a run without that feature is real code that produces
real mutants, and the instrumented tree compiles perfectly well, because the code holding them is
not part of it.

Left alone, every one of those mutants survives every run. No test can fail for code that was never
built, so they read as a test-suite gap that nobody can ever close — and there can be a great many
of them. On one measured crate, 378 of 2,290 survivors, 16.5%, sat behind a gate that did not hold,
which is thirty points of score.

So the `cfg` module answers which conditionally compiled code is in the build, and a mutant outside
it is reported `notbuilt` and excluded from the score, on the same reasoning as an unviable one: a
mutant that never ran is not evidence about the tests. It is named separately from both `unviable`
and `survived` so that a reader can see the feature set is the reason and decide whether to widen
it.

Feature resolution is computed by propagating to a fixed point over the workspace metadata already
loaded — a feature's own entries, `member/feature` cross-member enables, and the features a member
declares on another — rather than by asking cargo to resolve the graph, which would put the network
in the path of a decision that changes no verdict. Dev and build dependencies count, because the
schema is built through `cargo test`. Every ambiguity resolves toward *keeping* the mutant: this
filter exists to stop false survivors, and it must not become a way to lose real ones.

Two mechanisms answer overlapping questions here and it is worth keeping them apart. This one is
about what the compiler *read*; the rollback loop above is about what the compiler *rejected*.

## Stopping at the test that caught the mutant

A mutant is usually caught by one test, and once that test has failed the verdict is settled.
Everything the binary runs afterwards is paid for and cannot change the answer — and for a
well-tested codebase that is most of the suite, most of the time, on the majority of mutants.

Both harnesses announce each result as it happens, even when their output is piped, so the
announcement can be read as it arrives rather than after the process exits. The output is already
being drained on a dedicated thread for the stall detector below; the same thread watches for a
failure line, and the first one ends the run there. The saving is the whole tail of the binary.

The first announcement is kept rather than the last, because it is the test that convicted the
mutant and it is what the run would have reported had it read the output to exhaustion. The verdict
is therefore identical to the one a full run would have produced — only its cost differs.

This is sound only while the harness is the *only* thing writing to the stream being read. libtest
captures each test's output by default and replays it after every test has finished, so a failure
line appearing mid-run is libtest's own. Under `--nocapture`, `--show-output`, or an inherited
`RUST_TEST_NOCAPTURE`, that stops being true: a test's own writing lands among the harness's, and a
test that prints something shaped like a failure would convict a mutant the suite never caught.
That inflates the score, which is the worst direction to be wrong in — so where the stream is
interleaved the optimization is abandoned entirely, and the run waits for the exit code. A cheaper
answer is worth nothing if it is a different answer.

## The census: reusing the guard as a coverage probe

Reachability above is package-granular, and it is coarse. A crate whose tests all link the same
library runs every one of them against every mutant in it, and the great majority of those tests
cannot execute the mutated line at all. The obvious fix is per-test coverage, and the obvious way to
get it is `-C instrument-coverage` — a second full build of an already expensive tree, a second
artifact, an LLVM profile format to parse, and a mapping from source regions back to mutation sites.
That price is why most mutation testers either skip this or make it the whole architecture.

The schema makes it nearly free instead. Every mutation site is *already* a call — `gamma_rt::a(id)`
— placed at exactly the point a mutant would act, and identified by exactly the number a mutant is
named by. It is a coverage probe that has already been built. So the runtime gains a third mode: with
`GAMMA_CENSUS` naming a file, every guard answers `false`, which is the code the author wrote, and
records its own id. Run each test alone in that mode and the file it leaves behind *is* the set of
sites that test reaches. No second build, no profile format, and no region-to-site mapping, because
the ids are the sites.

**The relation is exact, not conservative.** A mutant at site *S* alters nothing before *S*
executes, so a test reaching *S* in the baseline reaches it under the mutant; and a test that does
not reach *S* never fires the mutant, so its execution is byte-identical to the baseline's and it
still does not reach *S*. Coverage-based test selection is normally an over-approximation that has to
be justified; here the two sets coincide by construction. The residue is nondeterminism — threads,
clocks, randomness, hash order. Deterministic reachability is overwhelmingly the common case and
the speedup is often orders of magnitude, so the census is the default; `--whole-test-binaries`
provides the conservative oracle when a suite cannot make that guarantee.

Recording it from the runtime is constrained by that crate being `no_std` with no dependencies, no
features, and no `build.rs`, because it is vendored into the instrumented tree as a single file. So
guards write only to a static atomic bitmap while the test is running. A life-before-`main`
constructor — `.init_array` on Linux, the platform equivalents elsewhere — registers `atexit` the
instant a census is asked for. At normal exit that handler closes bitmap recording, waits for every
in-flight update, opens the census file, scans the bitmap in ordinal order, and serializes the
reached IDs through 4 KiB `fwrite` batches. It then appends one reserved *seal*
(`u32::MAX - 2`) and calls `fflush`. The measured test therefore performs no filesystem operation;
all filesystem work is concentrated after its code has finished.

The reader believes a census only if it ends in an intact seal. A trailing seal proves that every
earlier batch was accepted first: an end-to-end completeness check rather than a hope. Anything that
breaks the chain withholds the seal — a failed `fopen`, a short or failed `fwrite`, an aligned
truncation, or an *abort* or signal that skips `atexit` altogether — and an unsealed or absent census
is discarded whole rather than believed in part, so a dropped killer record can never read back as
a survivor no test covered. The file is opened for appending so an unexpectedly reused path produces
an impossible stream rather than erasing evidence of the collision. A run that reaches nothing still
seals: an honest empty census is `[seal]`, never an empty file, so an absent file can only mean the
run failed.

The census rides on the same cached ordinal the guard already loads, as a reserved value
(`u32::MAX - 1`), rather than on a second static. The hot path therefore keeps its single relaxed
atomic load and pays one compare against a constant on a branch that is never taken in a real run.
Deduplication is a bitmap of a million sites, so however many threads or loop iterations reach a
site, the exit scan emits one record for it. A site past the end of the table sets an overflow flag;
the exit handler emits a marker for it, and that marker makes the *whole* census untrustworthy,
because an unrecorded site is indistinguishable from an unreached one and the failure has to be safe.

On the tool side the census is taken once, between the baseline and the sweep, by running each test
binary directly — one process per test, since a process-wide table cannot attribute two tests. This
is deliberately harness-independent: nextest is not asked to do it even when nextest is the harness,
because the attribution has to be the same either way.

**The invariant that governs every decision here is that an absent census means "run everything",
never "run nothing".** Only a positive measurement may narrow a run, and only a positive measurement
may turn a mutant into `uncovered` — which is the second thing the census buys, and a correction
rather than a saving: code no test reaches stops being blamed on the assertions. A binary that could
not be listed, whose census was cut short, or that produced a file the tool could not decode simply
makes no claim, and its mutants run its whole suite exactly as before.

Two smaller choices fall out of it. When the tests that reach a site are more than half the binary's
suite, the census declines to narrow at all: a long command line to skip a few tests is not worth
building, and "no claim" is already the safe answer. And the timeout budget is *not* re-based onto
the narrowed selection — a run of three tests carrying the whole binary's budget is generous rather
than tight, so leaving it alone can only avoid false hangs, never cause them.

## Detecting a hang without instrumenting for it

A mutant that hangs still has to be waited out, and the budget is the whole suite over again plus a
confirmation run at three times that. A run with a handful of hangs spends most of its time watching
them.

They can be cut off much sooner without any instrumentation, because libtest flushes `test foo ... ok`
per result even when its output is piped. So *silence* is the signal: output is drained on a
dedicated thread, which doubles as a progress watcher, and a binary that goes quiet for far longer
than the baseline ever did is presumed hung.

The budget is calibrated, not constant. A suite whose slowest test takes half a minute goes half a
minute quiet while perfectly healthy; a fixed budget would either accuse it or be useless for
millisecond unit tests. So the run measures the longest silence the *baseline* produced and allows
ten times that, floored at 5 seconds so scheduler noise cannot trip it. With no baseline there is nothing honest to calibrate against, so the
detector turns itself off.

The verdict names the last test the harness spoke about, which is a landmark rather than a diagnosis.
libtest runs tests in parallel and announces each one only when it finishes, so the name is whichever
test happened to finish last before the silence — not the one that is spinning, which by definition
has not finished. It narrows where to look; re-running the binary under `--test-threads=1` is what
turns it into a name.

> Draining that pipe on its own thread is also load-bearing for correctness, not just for speed.
> Piping a child's output and reading it only after the child exits would deadlock whenever the child
> writes past the pipe buffer — and libtest dumps the captured output of every *failing* test.
> Without concurrent pipe draining, the deadlock would manifest as a false timeout and score
> undetected survivors as caught.

## Where the time actually goes

Once the build leaves the inner loop, the cost model is:

```
total ≈ build + Σ (launch + suite_prefix_until_first_failure)
```

Three properties follow, and they are what `--estimate` and `--advice` report on:

- **The build is a fixed cost.** It is paid once no matter how many mutants there are, so *adding*
  mutants is comparatively cheap. This inverts the usual advice: with a per-mutant build you
  minimize the population; here you can afford a much richer mutator catalog.
- **Killed mutants are cheap; survivors are expensive.** A killed mutant stops at the first failing
  test. A survivor has to run every reachable binary to completion in order to prove that nothing
  caught it. A codebase with many survivors is slow *because* it is under-tested.
- **The suite's own speed is the multiplier.** It is multiplied by the population, so it dominates
  everything else. A slow test in the suite is not merely slow once.

`cargo gamma run --estimate` stops at the exact point a run stops measuring and starts waiting, and
projects the rest. Everything it reports before that point was measured, not guessed: the build
really built, the baseline really ran, and unviable mutants were really withdrawn.

One quantity is left, and it is the one that decides the answer: how many mutants hang. A mutant
that is judged pays for the tests it reached. A mutant that hangs pays for a whole budget it never
finishes and then for the confirmation run that budget is not believed without — and that budget has
a floor under it, so on a quick suite it is not a multiple of the tests but a constant far larger
than all of them. One hang can cost what several thousand judged mutants cost. Turning a loop
counter into an infinite loop is an ordinary mutation rather than an exotic one, so a projection
with no term for it is not conservative; it is wrong, and was measured two orders of magnitude
optimistic on a crate whose mutants hung.

Nothing available before the mutants execute can supply that share, so it is reported as the width
of the range rather than averaged away, and each end is labeled with the assumption that produces
it. A reader shown a bare interval learns only that the tool is unsure; a reader told that the width
is hanging mutants can lower the timeout floor or go and find them. It also reports a worst case,
because a CI job killed at the hour mark produces no report at all. The worst case counts the
confirmation run a suspected timeout or stall is put through, so it is a ceiling on *test time*
rather than a larger guess, and the projected range is capped at it. It is deliberately not a bound
on wall clock: per-mutant process launch, scheduling and reporting are not priced, because nothing
available before the run measures them, and the rendered line says so rather than letting the
figure be read as a promise.

`--advice` is the same measurements turned into prose after the fact: a list of findings, each one a
measured symptom, a named cause, a remedy, and — never omitted — what the remedy costs in signal.
Every mitigation available here trades information for time, and a recommendation that hides the
trade is worse than no recommendation, because it will be taken.

### In-process efficiency and Amdahl's law

The cost model above predicts that in-process work is Amdahl-capped: the run is subprocesses, so
tightening a hash map or removing an allocation cannot move a number that subprocess orchestration
sets. In-process optimizations are structured for algorithmic cleanliness and memory efficiency:
- The census aggregates into `FxHashMap` rather than a SipHash `std` map on integer keys.
- Discovery shares `Arc<str>` item paths and deduplicates candidates per span instead of allocating
  per candidate.
- Subprocess output scanners borrow failing test names directly from input buffers.
- The census worker pool draws from one flat `(binary, test)` queue across binaries, avoiding idle
  worker threads at binary boundaries.

The diagnostic bundle carries a `phases` object, timed once per phase as it runs. `phases.copy`
and `phases.preflight` are components of `build.elapsedMs`, with the compile time being the
remainder between them; `phases.baseline` restates `build.baselineMs`; `phases.census` and
`phases.sweep` are components of the testing window. The phases deliberately do **not** sum to
those totals — compiling sits between the copy and the baseline, and cache and scheduling
bookkeeping between the census and the sweep — because the point is to see *which* phase a slow
run spends time in, rather than reconciling numbers into an artificial partition.

Two counters provide visibility into execution trade-offs: `phases.census.walked` counts the census
subprocesses that actually ran (one per test per binary, less tests skipped when a binary spoils
partway), and `phases.sweep.probes` records whether killer hints successfully front-load verdicts.
Every phase that did not run is `null`, never `0` — an unrequested census did not take zero time;
it did not happen.

### Why compact strings are used selectively

`CompactString` stores up to 24 bytes inline and heap-allocates only beyond that limit. Measured
over the 10,496 mutants this workspace generates, the short, independently owned strings fit it
well:

| field | median | inlineable at 24 bytes |
|---|---|---|
| `id` | 12 B | 100% |
| report `mutator_name` | 17 B | 98.4% |
| `item_path` | 12 B | 94.5% |
| `replacement` | 5 B | 84.9% |
| `original` | 19 B | 59.3% |

The population therefore stores `id`, `original`, and `replacement` as compact strings. Candidate
collection constructs formatted replacements directly in that representation, so a short
replacement is not first allocated as a `String`, copied inline, and freed. The adjacent cache,
ordering-hint, killer, and report paths keep mutant IDs compact as well; report fields still
serialize as ordinary JSON strings.

Compact strings are not a blanket replacement for shared text. `file`, `package`, `mutator`, and
`item_path` remain `Arc` values because each has low cardinality and is reused by many mutants.
Inlining those values would copy their bytes into every mutant and grow each field from a
two-word `Arc` to a three-word compact string. In particular, a file opens a few hundred scopes and
can emit tens of thousands of candidates, so `item_path` must stay allocated once per scope rather
than once per candidate.

This choice primarily reduces retained allocations and peak memory. Discovery is a sub-second phase
dominated by `syn` parsing, and mutation runs are dominated by builds and tests, so compact strings
are not expected to materially change end-to-end run time.

## Choosing what to mutate

The mutator catalog is 106 mutators in 23 families. Each has a stable `family.transform` name —
`relational.gt_to_ge`, `arith.add_to_sub`, `stmt.delete_call` — and that one name is the entire
vocabulary: the command line, the config file, every suppression channel, the report, and the SARIF
rule IDs all use it. Eleven named presets (`@arithmetic`, `@boundary`, `@removal`, …) group them,
including one that is about migration rather than about a defect class: `@extreme` is a synonym for
`@all`, kept because existing scripts name it.

Because the catalog is a registry rather than a hand-written list, the reference tables in the README
are *generated* from it, and a test regenerates them and fails when they drift. A published
vocabulary that has gone stale is worse than none, because a reader who copies a name out of it gets
a usage error and no clue that the document was at fault.

Selection is a small left-to-right language over those names, where `!` removes: `@arithmetic,!bitwise`
means what it reads as. A selector may also be an academic alias, so a paper's `ROR` selects what
this tool calls the `relational` family. An unmatched selector is a hard error with a spelling
suggestion, never a silently empty set — the failure mode of pattern-based exclusion in other tools
is a filter that matches nothing and looks like it worked.

Mutators are written independently, so two of them can reach the same edit. Several converge on the
small integers: `0` becoming `1` is both `literal.int_increment` and `literal.int_to_one`, and `1`
becoming `0` is both `literal.int_decrement` and `literal.int_to_zero`. Two names for one edit is
still one edit — the same instrumented text, the same tests, the same verdict — so the second copy
buys nothing and weights its site twice in the score. A collector therefore records the span and
replacement of everything it emits and drops a later candidate that repeats one, which removed 347
of this workspace's 9 847 mutants without changing a verdict.

Whichever mutator gets there first keeps the name, and the order is chosen so that the surviving
name is the informative one: the perturbations are offered before the boundary values, because a
reader looking at `0` becoming `1` is checking an increment. Selection is consulted before any of
this, so a run that asks for only one of a colliding pair still gets its mutant.

## Suppression, and the directives that assert instead of hide

Mutants are withdrawn by three channels sharing one vocabulary — an attribute, a comment, and a
configuration rule:

```rust
#[gamma::skip(arith, reason = "fixed-point math, checked by proptest")]
fn scaled(a: i64, b: i64) -> i64 { a * b / 1000 }
```

```rust
// #[gamma::skip(arith)]
let total = a * scale + offset;
```

The second exists because attributes in statement and expression position are still unstable in
Rust. It is *character-for-character* the attribute form with `//` in front, so when expression
attributes stabilize, deleting two slashes turns it into real Rust. The attribute form is validated
by the compiler; the comment form is validated by the tool, which refuses to treat an unrecognized
directive as a suppression that quietly did nothing.

`skip` is one of three intents, and the other two do not suppress anything. `expect_survived` and
`expect_killed` still generate the mutant and still run it, and turn the outcome into an assertion
about the suite: a mutant expected to survive that gets killed is reported, and so is one expected
to be killed that survives, and either contradiction fails the run. `test_timeout_multiplier` (or
`#[gamma::test_timeout_multiplier]`) overrides the timeout budget factor for specific mutants
against their test binary baseline. That makes an annotation
self-correcting rather than a note that rots — when somebody finally writes the test, the run says
the comment is stale instead of leaving it to mislead the next reader.

Suppressed mutants are reported, with their reason, rather than hidden — for the same reason
unviable ones are. A score is only meaningful alongside the population it was computed over.

### Writing suppressions from a run

`cargo gamma suppress` closes the loop: it performs a run and writes directives into the source at
each eligible site. Two rules shape the module that does it.

**A surviving mutant is never eligible** — not by default, not behind a flag, not with a force
switch. A survivor is a real gap in the test suite, and suppressing it in bulk would remove the gap
from the score rather than from the code; the moment that is possible, every score the tool reports
becomes unfalsifiable. Only `timeout` and `unviable` verdicts can be reached this way, and a
surviving verdict has no spelling that reaches the module at all.

**Verify, do not assert.** After writing, discovery runs again and the suppressed set is compared
against the intent in both directions: every mutant that was meant to be suppressed must now be
suppressed, and nothing else may have become suppressed. If either check fails, every edit is
reverted. Writing source on a user's behalf is only defensible if the tool checks what it wrote.

## Naming a mutant

A mutant's identity is a BLAKE3 hash of its file, its item path, its mutator, the *normalized* text
of the site, which occurrence of that text within the item it is, and its `replacement_index`, which
records which of that mutator's replacements it applies — the last because one mutator can offer
several substitutions at a single
site, and two of them are different mutants with different verdicts. Every field is length-prefixed,
so no two different splits of the same bytes can collide.

An inherent implementation contributes its self type to the item path, while a trait implementation
uses `<Self as Trait>`. Thus same-named methods from different traits remain distinct when their
implementation blocks are reordered.

The digest is truncated to its first six bytes and rendered as twelve hex characters. Forty-eight
bits is short enough to read out of a report and paste into a suppression, and at the scale a
workspace reaches — tens of thousands of mutants — the chance of a collision is still negligible.

Deliberately absent: line and column.

Identity normalization removes Rust comments and collapses inter-token whitespace, while preserving
literal contents verbatim, including comment-shaped text and whitespace inside strings. SARIF names
the fingerprint `gammaMutantId/v3` so consumers use the trait-aware, literal-preserving
normalization contract.

That matters because mutant IDs are the join key for everything that spans runs — shard membership,
merged reports across nights, suppressions, SARIF alert fingerprints. If identity moved with line
numbers, running `cargo fmt`, or adding a function above, would reshuffle the entire population: a
nightly rotation would lose its coverage history and every security-tab alert would be dismissed and
immediately resurrected under a new name.

## Scoping to a change, and resuming

Exhaustive runs are for nightly jobs. Two mechanisms make a run affordable on a pull request, and
both narrow the population rather than the rigor applied to it.

`--in-diff` reads a unified diff — from a file, or from standard input, so `git diff origin/main |
cargo gamma run --in-diff -` is the whole idiom — records which lines each file's hunks touch, and
keeps only the mutants that land on them. What a change did not touch cannot have been broken by it.

`--incremental <no|build|full>` (default: `full`) controls what an incremental run reuses from the
previous run's record (`target/gamma/last-run.json`). In `full` mode, mutants an earlier run already settled —
killed, unviable or ignored — are skipped only when the compiler, Cargo configuration, execution
policy, and complete pre-execution snapshot of Cargo's workspace inputs still match. Timeouts are
always rerun. In
`build` mode, only compiler unviability is reused. In `no` mode, a completely cold run executes.
Survivors and uncovered mutants are always retried, because the tests may have grown since, and a
survivor is the one verdict that a later run can legitimately overturn.

These compose with sharding, and neither is a substitute for the other: a shard is a slice of
everything, a diff is a slice of what changed, and an incremental run is a slice of what is not yet
answered.

A carried-forward kill is a claim about the *tests*, and a mutant's identity hashes only
the production code. Delete or edit the test that made a kill and the next run might otherwise adopt
the kill and report coverage that no longer exists. To prevent this, gamma stores a BLAKE3
pre-execution snapshot of every workspace file Cargo can consult, excluding only generated target
and scratch trees. When `RunRecord::settled` revalidates verdicts, the entire snapshot must still
match, and the unchanged declaring test file must still declare the exact killer identity.

Which tests exist is answered by scanning the sources for `#[test]` functions, over every target
including the integration tests the run never mutates. The obvious alternative — asking a harness
to `--list` — is available only after a build, which is after incremental adoption has already decided what
to skip, so it answers too late to be of use. The scan is an approximation, and it is spent
deliberately in one direction: tests generated by macros are invisible to it, so their kills are
re-run rather than wrongly kept. A kill with no killer recorded is treated the same way: `killedBy`
is optional in the `mutation-testing-elements` schema, so a report from another tool may carry a
kill it cannot attribute, and re-running costs time rather than accuracy.

## What a run remembers

Three things carry knowledge from one run into the next, and the axis that separates them is
**durability**, not mechanism:

| | Where | Lifetime | Adopted |
|---|---|---|---|
| Run record | `target/gamma/last-run.json` | The scratch tree | Unviability in `build` mode; unviability & settled verdicts in `full` mode (validated by complete workspace snapshots) |
| Hints artifact | `.cargo/gamma-hints.json` | Version control | Only what cannot move a score, always |
| Skip directive | The source | Version control | Always |

**One store.** The record holds an entry per mutant, keyed by content id, under the source file the
mutant lives in — the file's digest and length are what invalidate it. Above that sits the build
context: the features, profile, rustflags, target, complete applicable Cargo configuration, extra
Cargo arguments, toolchain, test selection, execution policy, and this tool's own version, because
the id's *meaning* is a property of this tool.

**The context is ten terms, not one digest.** Each is hashed on its own, and each *tier* of the
record names the terms it actually depends on. Unviability is a claim about what compiles, so it
requires every term and the digest of the file it was found in; anything less and a mutant carried
as unviable that would compile today leaves the denominator silently, turning a real gap into a
better score. A kill is a claim about the test suite and requires every verdict term, including the
compiler and harness policy: either can change a result. A killer probe and a build-ordering hint
require none. A single opaque envelope could not express any of that without over-invalidating the
hints.

**Three levels of trust, and they are not a detail.** Unviability moves no verdict: an unviable
mutant is outside the score whether it is rediscovered or carried, so adopting it can only cost
time. A kill is a claim about the test suite. Adopting one settles part of the score, and gamma
safely allows this in `--incremental full` (the default) only after the complete workspace snapshot
and exact, unchanged declaring test file agree. `--incremental build` and `--incremental no` provide
explicit opt-outs to restrict or disable caching. Timeouts are never carried: current scheduling and
baseline timing must revalidate them.

**A third level that is not trusted at all.** The record also holds killer probes — which test
caught each mutant, and in which binary — and, below even those, a build order. Neither is ever
believed. A probe is checked by running the named test, and one that does not convict costs a single
filtered process. A build-ordering hint is checked by the compiler: the mutants an earlier run could
not compile are spliced in and offered to the compiler first, on their own, so that a genuinely
unviable one is blamed with nothing else masking it. Every one of them is still built and still
judged; a hint that is wrong produces a mutant that compiles, stays live, and is swept exactly as if
it had never been named.

**Which is what lets stale unviability keep earning.** Rounds are the only cost in the model that is
both large and inherently sequential — test time parallelizes across jobs, convergence does not — so
throwing unviability away on a context mismatch is expensive. The unsafe use of it is as a *filter*;
using it to decide what to build *first* is safe under any context, because being wrong about the
order costs nothing but the order. So the exact-context tier still filters, and the stale tier is
demoted to an order rather than discarded.

The run reports what that bought as two facts and no more: how many mutants the probes front-loaded,
and how many of those the compiler then refused. There is deliberately no "rounds saved" figure. That
number is the length of a convergence that never ran, over a population the compiler was never shown
in that shape, and printing a model of it as if it were a measurement would make every other figure
in the diagnostic bundle worth less.

**A file the workspace can check in.** Everything above lives under `target/`, and CI deletes that
on every run — so every CI run is a cold run, with an empty killer map and an unguided build, on
exactly the runs that cost the most. `cargo gamma hints` promotes the two tiers that cannot move a
score into `.cargo/gamma-hints.json`, which is reviewed, committed, and consulted automatically by
every later run with no flag to remember. It is a promotion rather than a `cp`: the command owns the
format version and the provenance, joins the record against the population as it stands now so the
file does not grow forever, orders entries by source file and then by content id so its diff is
reviewable, writes atomically, and reads back what it wrote. Verdicts are refused entry — a carried
kill adopted silently out of version control would make every reported score unfalsifiable — and
unviability is admitted only after being demoted to an ordering hint, because a committed envelope
will differ from the run reading it almost always.

Reading the artifact is best-effort to the point of indifference: missing, truncated, from another
version, or written by something else entirely all mean the same thing, which is no hints. Writing
it is not, because a promotion is something somebody asked for and a promotion that quietly did
nothing surfaces as an unexplained slow CI run weeks later.

**What the content id does and does not protect.** A moved or edited site stops matching, which is
the safe direction, and it needs no help. Anything derived from *other* code has no such protection:
a kill names a test that lives somewhere else entirely, so it is revalidated against an index of the
`#[test]` functions the sources declare — see *Scoping to a change, and resuming* for why that index
is a scan rather than a harness query.

**Suppression stays outside all of it.** `cargo gamma suppress` writes into the source, where the
claim is reviewed, committed and survives a clean checkout. Folding it into the record would make a
deliberate decision indistinguishable from a cache entry, and the whole value of a directive is that
it outlives any cache. Correspondingly, the record must always be safe to delete: `rm -rf
target/gamma` may cost time and must never cost signal. The same holds for the checked-in artifact —
deleting `.cargo/gamma-hints.json`, or letting it go stale, costs time and never signal — which is
what makes it safe to put a generated file in version control at all.

## Sharding across nights

Even at one build per run, mutation testing a large workspace exhaustively does not fit in a nightly
CI budget. The population is therefore divisible into `N` shards, with one shard run per night.

The assignment uses **jump consistent hashing** rather than `hash % N`. The difference shows up when
the shard count changes: with a modulus, going from 8 shards to 9 reshuffles roughly 8/9 of all
mutants; with jump consistent hashing, only the fraction that mathematically must move does. Shard
membership is something a team can reason about across a config change, instead of a fresh random
assignment every time somebody edits the CI file.

## Merging what the shards found

Because identity is content-addressed and membership is stable, per-night reports merge into one
score for the whole workspace. The merge is a union by mutant ID, keeping the most recent verdict
for each, and it reports three things a single run cannot:

- **Never tested** — an identity the rotation has not reached yet. A mutant whose code has since
  been edited has a new identity, so it reappears here rather than inheriting the verdict its
  predecessor earned.
- **Stale** — a verdict older than the freshness window. It is still counted, and never claimed to
  be fresh. Discarding it would shrink the denominator, which raises the score by forgetting rather
  than by testing.
- **Withdrawn** — an identity that a newer input no longer states. This is the asymmetric one: a
  union by itself can never drop anything, so withdrawal requires an input that describes a
  *complete* population, which means an unsharded run or listing. A sharded report describes only
  its own slice and therefore never withdraws anything.

## What a verdict means

| Outcome | Name | Meaning | In the score |
|---|---|---|---|
| `Killed` | `killed` | A test failed while this mutant was active | detected |
| `Timeout` | `timeout` | The budget or the stall detector cut it off | detected |
| `OutOfMemory` | `outofmem` | The run passed the memory ceiling derived from its own baseline | detected |
| `Survived` | `survived` | Every reachable test passed | undetected |
| `NoCoverage` | `uncovered` | No test binary links this code | undetected |
| `CompileError` | `unviable` | The mutation does not compile | excluded |
| `Ignored` | `ignored` | Withdrawn by a directive, attribute or config rule | excluded |
| `NotBuilt` | `notbuilt` | Conditional compilation kept this file out of the build | excluded |

The middle column is the serialized name — what a JSON report carries and what a log can be grepped
for. The console and the report now agree: both say `killed` and `survived`.

The score is detected over detected-plus-undetected. Unviable, suppressed and never-built mutants
are excluded from both — they were never tested, and counting them either way would be a statement
about the tests that the tests did not make. The user-facing statement of this, with the score
written out, is in [the README](../README.md#verdicts-and-the-score); the table above adds the
internal enum each name corresponds to.

There are two numbers here, not one, and keeping them apart is what makes the `--min-score` gate
safe. `score` is the *printable* value: for an empty population it is 100%, because a run that
caught everything it tested has nothing to apologize for, and both `Summary` and `Merged` agree on
that so `run` and `merge` can never report opposite figures for "nothing scored". `scored` is the
*gradeable* value, an `Option<f64>` that is `None` when the valid population is zero. Every gate
reads `scored`, never `score`, so an empty population fails the gate structurally rather than
because a placeholder happened to be unflattering — handing 100% to a threshold is the one outcome
a mutation-testing gate must never produce by accident.

The failing message renders both figures through a shared helper that prints just enough precision
to keep them apart. The comparison is always full-precision `f64`; it is only the *message* that
would otherwise round, and a gate that reports "mutation score 80.0% is below the required 80.0%"
sends someone to debug the one output whose whole job is to explain a CI failure.

An out-of-memory verdict is the one detection incremental runs do not carry forward. Linux reaches it
from an `oom` or `oom_kill` event recorded against the invocation's own cgroup — the kernel saying it
refused the workload — and Windows assigns the child to its job object while it is still suspended,
so the ceiling is in force from the first allocation. But the Windows *rule* is still
`PeakJobMemoryUsed >= limit`: a reading compared against a number rather than a reported refusal. A
job capped at the limit cannot peak above it, so equality is strong evidence and not proof, and until
Windows reports the refusal itself the verdict is re-run rather than frozen. It is re-run on every
platform rather than only on Windows, because a report is a portable artifact and a mutant that
iterated differently depending on which machine read it would be worse than one re-run needlessly.
The cost is knowingly accepted: these are by definition the runs that use the most memory, and so the
expensive ones to repeat.

The report has fewer statuses than this table has outcomes, so three of them share `Ignored`:
a deliberate suppression, a flake, and a mutant whose build the run gave up on. The better-named
alternatives — `NoCoverage` for the last, `RuntimeError` for a flake — sit on the other side of the
schema's denominator, so exporting them there would make the viewer's score disagree with the printed
one, which is the disagreement these outcomes exist to remove. The distinction is carried in
`statusReason` instead, behind the prefixes `flaky: ` and `not built: `, and counted in the report's
`config.notBuilt` so that a CI job can total them without walking every mutant. Neither is settled:
incremental runs re-run both, because a run that established nothing about a mutant must not freeze it
into every run after it.

Underneath this sits a second, narrower enum: the verdict of one test binary against one mutant —
passed, failed, timed out, stalled, over its memory limit, or unmetered. Several of those fold into
one `Outcome` once every reachable binary has spoken, which is why the two exist separately.

## Showing that a long build is moving

The design trades many builds for one, which concentrates almost all of a run's compilation into two
places: the instrumented tree, and then the baseline test binaries. On a large workspace each is
minutes long, and for that whole time the run has nothing of its own to say — there are no verdicts
yet. A run that had wedged and a run that was working looked identical, which made the tool's most
expensive phase also its least legible one.

The fix is not to invent a measure of doneness but to take cargo's. Cargo holds the unit graph, so
it already knows both the numerator and the denominator; it merely declines to draw the bar when its
standard error is not a terminal. `CARGO_TERM_PROGRESS_WHEN=always` overrides exactly that, on stable and without a pseudo-terminal,
and it composes with `--message-format=json` because the two travel on different streams: cargo's
standard output carries the JSON the rollback loop parses, and its standard error carries the
human-readable progress prose. The progress display renders that line where its own bar goes.

Two consequences are worth stating. The bar advances by compilation units, not by time, so it moves
unevenly — `syn` and a leaf crate count the same. That is accepted deliberately: it makes this bar
wrong in precisely the way cargo's own bar is wrong, which is the way users are already calibrated
to. And the line is cosmetic, so failing to recognize it is harmless — an unrecognized line falls
through to being treated as prose, and the failure mode is a bar that does not appear rather than a
garbled one. This is the opposite of the test-output parsers, where a misparse would move the score.

Compiler errors are surfaced as they arrive, from the JSON stream rather than the prose one, because
that is where rustc's diagnostics travel. Only errors: an instrumented tree emits a great many
warnings, and not one of them says whether the build will produce the binaries the run needs. Only
the first few, and each distinct message only once — errors during this build are routine rather
than exceptional, since the rollback loop withdraws unviable mutants precisely *by* letting them
fail to compile, and a first round on a large workspace can legitimately produce hundreds. Showing
them all would bury the run in errors it is about to handle by itself. Each is condensed to a line
with its code and primary location, because the rendered form carries the source snippet and the
underlines, which is what you want when you are fixing an error and far too much when the point is
only to say that the build is working through something.

`--show-build` is the escape valve, and it exists because every filtering decision above is a guess
about what matters. It passes cargo's output through unfiltered and suppresses the bar while it does
— two displays redrawing the same line would garble both. It is what to reach for when the build
itself, rather than any mutant, is what is going wrong.

One implementation detail is load-bearing and does not look it. Every redraw is composed in full and
handed to the stream in a single `write_all`, never assembled with `write!`. The diagnostic stream is
unbuffered, and `write!` with a format string issues one write per literal and per interpolated
argument, so `write!(stream, "\r\x1b[2K{line}")` is two syscalls: the erase lands first and leaves
the row blank, and the text lands second. The Windows console paints each write as it arrives, so
multiple syscalls would draw a blank row and then the text on every redraw, causing rapid flickering.
Composing first costs one short-lived allocation against a hundred-millisecond interval and makes
the update atomic as far as any console is concerned, so the display goes through a single helper
that takes a finished string. An unchanged borrowed line is also not repainted at all, avoiding
redundant erasures and repaints.

## Projecting a run: the reporting surfaces

One run produces one set of verdicts and four renderings of them, and each surface is shaped by
where it is read.

**The console** follows `cargo build`'s layout deliberately — a right-aligned verb column, a live
progress line redrawn in place — because a tool that prints like the tool beside it needs no
explanation. `--progress` and `--color` are separate flags, both `auto`, because a CI log wants the
color off *and* the redraw off for different reasons.

Every measured run also truncates `testing_progress.log` in the Gamma scratch directory after it
acquires that directory's run lock. Each mutant verdict is appended there in the console's
right-aligned outcome format and flushed immediately, including ordinary kills and other outcomes
the live console suppresses. The file is therefore an interruption-recovery journal rather than a
final report: it may end anywhere, but every complete line names a verdict the run had already
reached. It contains no terminal color or redraw escapes.

**The JSON report** is the `mutation-testing-elements` interchange format rather than a private
schema. That single decision is what supplies the report viewers, the Azure DevOps extension and the
GitHub extension without writing any of them.

**The HTML report** is a single self-contained file, with the viewer bundle and the results both
inlined: no CDN, no fonts, no fetch, no network at all, so it opens from a CI artifact or an air-
gapped machine. `--html-external` trades that for a much smaller file that needs the network.

**SARIF and CI annotations** put findings where the reviewer already is. All of these publish
**survivors only**: a killed mutant is the tool working, and reporting it would bury the signal
under its own success. Both are capped at what GitHub actually accepts — ten annotations per step,
five thousand SARIF results — and a run that hits a cap says so rather than silently truncating.
SARIF rule identifiers are the stable mutator names and results are fingerprinted by mutant ID, so a
team's dismissal of a mutator keeps applying to code written next year, and an alert follows its
code through reformatting.

## Arriving from another mutation tester

Source compatibility is deliberately refused alongside configuration compatibility.
`#[mutants::skip]`, its comment form, and the `#[cfg_attr(test, mutants::skip)]` spelling are all
ignored: they belong to another tool, and reading them would mean this tool's exclusions were partly
written in a vocabulary it does not own — one whose selectors, arguments and defaults it cannot
promise to keep meaning the same thing. Ignoring rather than rejecting them keeps an
already-annotated codebase building; its sites come back as mutants until each attribute is
rewritten as `#[gamma::skip]`.

`.cargo/mutants.toml` is never read. It is a different schema for a different tool with a different
catalog, so honoring it silently would let a foreign file's exclusions quietly change which mutants
this one skips, and therefore quietly change the score. `cargo gamma migrate` translates it once,
visibly: nothing is dropped, a key with no equivalent becomes a `TODO` carrying its rendered
semantic TOML value (not its comments or formatting), every translated line names the key it came
from, and an existing `gamma.toml` stops the migration rather than being overwritten. Command lines
are translated too, which is what turns an existing CI workflow into a one-line change.

## What this design costs

No design is free, and these are the prices:

- **The build is bigger.** Instrumented source is several times larger than the original, and
  compiling it takes correspondingly longer. This is a fixed cost paid once, traded against a
  variable cost paid per mutant, which is why it wins by orders of magnitude at scale — and why it
  wins by less on a tiny crate.
- **A pathological mutant can break the whole build.** The rollback loop handles it, but each round
  costs another compile. This is the price of putting the population in one tree.
- **Guards perturb the code under test.** They add branches, which can affect inlining and code
  layout. Semantics are preserved, but a benchmark run under instrumentation is not measuring your
  real binary.
- **Exactly one mutant per process.** Testing several at once would confound their verdicts, so the
  process launch cannot be amortized further. It is the floor this design leaves behind.
- **Mutants are compiled, so they must be well-typed.** A textual tool can generate a mutation that
  merely fails at test time; here it fails at build time and is withdrawn. That is mostly a feature —
  an unviable mutant tests nothing — but it does mean the catalog is bounded by what the type system
  will admit.
- **Memory enforcement is not portable.** It rests on cgroup v2, so it is a Linux facility. Elsewhere
  the run reports enforcement as unsupported rather than substituting a weaker mechanism under the
  same name, which means the `OUTOFMEM` verdict is one that some hosts can never produce.
