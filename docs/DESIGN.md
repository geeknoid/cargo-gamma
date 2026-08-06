# How cargo-gamma works

This document explains the machinery: what the tool does to your code, why that makes it fast, and
what the design costs. It is not a tour of the source. If you want to know *why the numbers come out
the way they do*, this is the right file.

## Contents

- [The problem](#the-problem)
- [The idea: one build, every mutant](#the-idea-one-build-every-mutant)
- [Encoding a mutant](#encoding-a-mutant)
- [Making it compile: the rollback loop](#making-it-compile-the-rollback-loop)
- [The scratch tree](#the-scratch-tree)
- [Running the population](#running-the-population)
- [Not running what cannot matter](#not-running-what-cannot-matter)
- [Detecting a hang without instrumenting for it](#detecting-a-hang-without-instrumenting-for-it)
- [Where the time actually goes](#where-the-time-actually-goes)
- [Choosing what to mutate](#choosing-what-to-mutate)
- [Naming a mutant](#naming-a-mutant)
- [Sharding across nights](#sharding-across-nights)
- [What a verdict means](#what-a-verdict-means)
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

Each package is named before its files are read and reports what survived compilation on the same
line once its build is done. Scanning and compiling a large crate is the longest a run goes without
saying anything, and what makes that wait legible is knowing whose wait it is; the count has to wait
for the build, because until then there is no way to know which mutants were viable.

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
needs a judgement call; there the innermost guarded site containing the diagnostic is blamed.

Each round removes every mutant blamed in that round, not one, so the loop converges in a handful of
rounds rather than one round per unviable mutant. It is capped at 32 rounds, and a diagnostic that
cannot be attributed to any mutant is a hard error naming the scratch tree — that means the tool
broke your code, and quietly withdrawing mutants until the symptom disappeared would be the wrong
answer.

Withdrawn mutants are counted as `unviable` rather than hidden. They are a fact about the code, and
a tool that silently drops the ones it found inconvenient is reporting a score about a population it
will not name. The count is unconditional for that reason; the list behind it is not, because a
large workspace withdraws thousands and printing them all buries the survivors that are the point.
`--unviable` asks for the list.

Each round rewrites only the files whose mutants changed. Cargo decides what to rebuild from mtime
rather than content, so writing a file back byte-for-byte would recompile its crate and everything
downstream of it — which meant a round that withdrew thirty-four mutants rebuilt the entire
workspace for them. Comparing before writing took a sixteen-crate workspace from two minutes to
seventy-four seconds without changing a single verdict.

## The scratch tree

The instrumented sources are never written where you work. The workspace is copied to
`target/gamma/tree`, and the guard runtime is vendored into it — written to disk from a copy
embedded in the tool itself, so it cannot drift from the version the guards were generated against,
and so nothing is fetched from the network. `--scratch-dir` moves the whole of `target/gamma`
elsewhere, for a workspace on a slow or full disk, or to give two concurrent runs trees of their
own.

The copy is not a naive recursive walk. It is parallel, it honours the workspace's `.gitignore`
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

The guard runtime is deliberately a zero-dependency crate. It is injected into *your* dependency
graph, and a dependency, a feature, or a build script there could perturb feature unification in
your tree — changing what your code compiles to, and therefore what your tests prove. Zero
dependencies is a correctness requirement, not a preference.

## Running the population

After the build, the run measures before it guesses:

**The baseline.** The suite runs once with no mutant active. If it is already red, everything after
it is meaningless — a failing test kills every mutant and the score comes out perfect. The baseline
also produces the two measurements everything downstream is calibrated from: how long the suite
takes, and the longest it legitimately goes silent.

**The timeout.** A mutant can turn a loop into one that never ends, so every run needs a budget.
That budget is derived from the measured baseline (1.2× by default, with a 20-second floor) rather
than fixed, because a constant is either too tight on a slow machine or useless on a fast one. A
mutant that exceeds it counts as *detected*: a hang is a behavior change the suite noticed, even
though it noticed expensively. A mutant that runs out its budget is not believed on the first try —
it is given three times the budget once more before the verdict is recorded, because a loaded
machine can starve a healthy test for longer than a tight budget allows and a false timeout is
scored as a kill.

**Parallelism.** Mutants are distributed across worker threads — one per core by default — pulling
from a shared queue. Each worker launches a test binary with `GAMMA_ACTIVE` set to its mutant's
ordinal. Since selection is per process and the binary is read-only, the workers share everything
and coordinate on nothing but the queue index.

**Early exit.** A mutant is killed by the *first* test that fails, so a binary that fails ends that
mutant immediately. Combined with the ordering below, a mutant that any test kills is usually killed
by the cheapest binary that could have killed it.

## Not running what cannot matter

Two filters cut the remaining term without changing a single verdict.

**Reachability.** Rust cannot call code it does not link. A test binary built from a package that
does not depend — directly or transitively — on the package a mutant lives in can never execute that
mutant. Running it there costs a full suite and can only produce the answer it already had. The
dependency closure is computed from *declared* dependencies rather than a resolved graph, so it
costs nothing extra to obtain.

The filter fails open: if either package cannot be identified, it is assumed reachable. A missed skip
costs a little time; a wrong skip would hide a real gap in the test suite.

A mutant that *no* binary can reach is reported `uncovered` rather than `survived`. Both count
against the score identically, but they call for different responses — a survivor means a test ran
and did not notice, while an uncovered mutant means no test exists at all.

**Doctests are outside the model.** `cargo test --doc` is never run, so a doctest neither kills a
mutant nor contributes to the baseline. This follows from the schema rather than sitting beside it:
the whole economy of the approach is one build and one process launch per mutant, and `rustc`
compiles and links a separate binary per doctest. Admitting them would restore a per-mutant compile
for the tests that typically assert the least. The visible effect is that coverage carried only by
doc examples shows up as `uncovered`, which makes the score a lower bound on such a crate.

**Cheapest first.** Each binary is timed during the baseline, and they are tried in ascending order
of that time. This changes no verdict, only what a verdict costs.

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
ten times that, floored at 5 seconds so scheduler noise cannot trip it and capped by the wall
timeout it exists to pre-empt. With no baseline there is nothing honest to calibrate against, so the
detector turns itself off.

The verdict names the last test the harness spoke about, which is a landmark rather than a diagnosis.
libtest runs tests in parallel and announces each one only when it finishes, so the name is whichever
test happened to finish last before the silence — not the one that is spinning, which by definition
has not finished. It narrows where to look; re-running the binary under `--test-threads=1` is what
turns it into a name.

> Draining that pipe on its own thread is also load-bearing for correctness, not just for speed.
> Piping a child's output and reading it only after the child exits deadlocks once the child writes
> past the pipe buffer — and libtest dumps the captured output of every *failing* test, so any mutant
> that breaks a talkative test hits it. The failure mode was ugly: the mutant looked like a timeout,
> a timeout counts as detected, and survivors were therefore scored as caught.

## Where the time actually goes

Once the build leaves the inner loop, the cost model is:

```
total ≈ build + Σ (launch + suite_prefix_until_first_failure)
```

Three properties follow, and they are what the `estimate` and `advise` subcommands report on:

- **The build is a fixed cost.** It is paid once no matter how many mutants there are, so *adding*
  mutants is comparatively cheap. This inverts the usual advice: with a per-mutant build you
  minimize the population; here you can afford a much richer operator catalog.
- **Killed mutants are cheap; survivors are expensive.** A killed mutant stops at the first failing
  test. A survivor has to run every reachable binary to completion in order to prove that nothing
  caught it. A codebase with many survivors is slow *because* it is under-tested.
- **The suite's own speed is the multiplier.** It is multiplied by the population, so it dominates
  everything else. A slow test in the suite is not merely slow once.

`cargo gamma estimate` stops at the exact point a run stops measuring and starts waiting, and
projects the rest. Everything it reports before that point was measured, not guessed: the build
really built, the baseline really ran, and unviable mutants were really withdrawn. The only thing
left uncertain is how much of the suite a killed mutant reaches, which it names as a range rather
than folding into one confident number. It also reports a worst case, because a CI job killed at the
hour mark produces no report at all. The worst case counts the confirmation run a suspected timeout
or stall is put through, so it is a ceiling a run cannot exceed rather than a larger guess, and the
projected range is capped at it.

## Choosing what to mutate

The operator catalog is 66 mutators in 11 families. Each has a stable `family.transform` name —
`relational.gt_to_ge`, `arith.add_to_sub`, `stmt.delete_call` — and that one name is the entire
vocabulary: the command line, the config file, every suppression channel, the report, and the SARIF
rule IDs all use it. Nine named profiles (`@arithmetic`, `@boundary`, `@removal`, …) group them.

Selection is a small left-to-right language over those names, where `!` removes: `@arithmetic,!bitwise`
means what it reads as. An unmatched selector is a hard error with a spelling suggestion, never a
silently empty set — the failure mode of pattern-based exclusion in other tools is a filter that
matches nothing and looks like it worked.

Mutants are withdrawn by three channels sharing one vocabulary:

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

Suppressed mutants are reported, with their reason, rather than hidden — for the same reason
unviable ones are. A score is only meaningful alongside the population it was computed over.

## Naming a mutant

A mutant's identity is a BLAKE3 hash of its file, its item path, its mutator, the *normalized* text
of the site, which occurrence of that text within the item it is, and its `replacement_index`, which
records which of that mutator's replacements it applies — the last because one mutator can offer
several substitutions at a single
site, and two of them are different mutants with different verdicts. Every field is length-prefixed,
so no two different splits of the same bytes can collide.

The digest is truncated to its first six bytes and rendered as twelve hex characters. Forty-eight
bits is short enough to read out of a report and paste into a suppression, and at the scale a
workspace reaches — tens of thousands of mutants — the chance of a collision is still negligible.

Deliberately absent: line and column.

That matters because mutant IDs are the join key for everything that spans runs — shard membership,
merged reports across nights, suppressions, SARIF alert fingerprints. If identity moved with line
numbers, running `cargo fmt`, or adding a function above, would reshuffle the entire population: a
nightly rotation would lose its coverage history and every security-tab alert would be dismissed and
immediately resurrected under a new name.

## Sharding across nights

Even at one build per run, mutation testing a large workspace exhaustively does not fit in a nightly
CI budget. The population is therefore divisible into `N` shards, with one shard run per night.

The assignment uses **jump consistent hashing** rather than `hash % N`. The difference shows up when
the shard count changes: with a modulus, going from 8 shards to 9 reshuffles roughly 8/9 of all
mutants; with jump consistent hashing, only the fraction that mathematically must move does. Shard
membership is something a team can reason about across a config change, instead of a fresh random
assignment every time somebody edits the CI file.

Because identity is content-addressed and membership is stable, per-night reports merge into one
score for the whole workspace, with each verdict carrying its own age. The merge reports staleness
rather than discarding old verdicts: dropping them would shrink the denominator, which raises the
score by forgetting rather than by testing.

## What a verdict means

| Outcome | Meaning | Counts as detected |
|---|---|---|
| `caught` | A test failed while this mutant was active | yes |
| `timeout` | The budget or the stall detector cut it off | yes |
| `survived` | Every reachable test passed | no |
| `uncovered` | No test binary links this code | no |
| `unviable` | The mutation does not compile | excluded |
| `ignored` | Withdrawn by a directive, attribute or config rule | excluded |

The score is detected over detected-plus-undetected. Unviable and suppressed mutants are excluded
from both — they were never tested, and counting them either way would be a statement about the
tests that the tests did not make.

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
