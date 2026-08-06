# cargo-gamma

Fast mutation testing for Rust.

[![Crate](https://img.shields.io/crates/v/cargo-gamma.svg)](https://crates.io/crates/cargo-gamma)
[![Docs](https://docs.rs/cargo-gamma/badge.svg)](https://docs.rs/cargo-gamma)

* [Summary](#summary)
* [Installing](#installing)
* [Using](#using)
* [Scoping a run](#scoping-a-run)
* [Suppressing mutations](#suppressing-mutations)
* [Sharding](#sharding)
* [Controlling a run](#controlling-a-run)
* [Reports](#reports)
* [Continuous integration](#continuous-integration)
* [Configuration](#configuration)
* [Fixing timeouts and unviable mutants](#fixing-timeouts-and-unviable-mutants)
* [Coming from cargo-mutants](#coming-from-cargo-mutants)
* [Hangs](#hangs)
* [Diagnosing a slow run](#diagnosing-a-slow-run)
* [What a mutant is not run against](#what-a-mutant-is-not-run-against)
* [Status](#status)
* [Contributing](#contributing)
* [License](#license)

Reference material lives beside this file:

* [docs/OPERATORS.md](docs/OPERATORS.md) — every mutator, by family, with its academic alias.
* [docs/PROFILES.md](docs/PROFILES.md) — the named mutator sets, and which to reach for.
* [docs/SUPPRESSION.md](docs/SUPPRESSION.md) — the directive grammar, where a directive may go, and how the five suppression channels differ.
* [docs/CONFIGURATION.md](docs/CONFIGURATION.md) — every `.cargo/gamma.toml` key and the flag it corresponds to.
* [docs/DESIGN.md](docs/DESIGN.md) — how it works internally, and why it is faster.

## Summary

Line coverage tells you a line ran. It does not tell you that anything would have noticed if the
line were wrong. Mutation testing answers the second question directly: change the code in a way
that must break something, then see whether the test suite complains. A change nothing notices is a
gap in the suite, pointed at with a file, a line, and the exact edit that went undetected.

`cargo-gamma` is a mutation testing tool for Rust. It covers the ground
[cargo-mutants](https://github.com/sourcefrog/cargo-mutants) covers, with a considerably larger
catalog of mutation operators and a substantially faster execution model.
[docs/DESIGN.md](docs/DESIGN.md) explains how it works internally and why it is faster.

Two things distinguish it:

**A wider catalog.** Replacing a function's return value finds a lot, but it walks straight past an
off-by-one in a comparison, an inverted condition, a dropped side effect, or a `+` that should have
been a `-`. `cargo-gamma` mutates relational operators, arithmetic, bitwise and shift operators,
compound assignment, logical connectives, branch conditions, unary operators, and literals, as well
as function return values.

**One build instead of thousands.** The conventional approach rebuilds the crate once per mutant,
which is why mutation testing has a reputation for taking all night. `cargo-gamma` compiles every
mutant into a single binary and selects one at runtime, which turns a multi-minute rebuild into a
process launch. [docs/DESIGN.md](docs/DESIGN.md) works through what that costs and what it buys.

## Installing

```bash
cargo install cargo-gamma
```

## Using

Run against the current workspace:

```bash
cargo gamma run
```

`run` copies the workspace to a scratch tree, compiles every selected mutant into one set of test
binaries, measures a baseline with no mutant active, and then runs the suite once per mutant. Each
mutant that no test noticed is reported with its file, line, and the exact edit that went
undetected:

```text
      MISSED src/ledger.rs:41:9: delete self.audit.push(entry); [stmt.delete_call]

     Summary 83 mutants (51 caught, 32 missed, 0 timed out, 0 out of memory, 0 uncovered => 61.4%)
```

The run ends on one line. All five verdicts are always named, zero or not, so the line keeps a
shape you can scan rather than read, and they sum to the population in front of them. Mutants that
were deliberately held back — suppressed, outside the current shard, not built into this feature
configuration, or already settled by an earlier report — are appended only when there are any.
Mutants that could not be compiled are withdrawn automatically and are not counted here;
`--unviable` lists them.

```text
     Summary 303 mutants (294 caught, 7 missed, 2 timed out, 0 out of memory, 0 uncovered => 97.7%), 12 suppressed
```

See what would be tested without testing it:

```bash
cargo gamma list mutants
cargo gamma list ops
cargo gamma list files
```

Choose which mutators to apply. A selector is a mutator name, a family prefix, a profile, or an
academic alias; `!` removes from the set, and selectors apply left to right:

```bash
cargo gamma run --ops relational
cargo gamma run --ops @arithmetic,!bitwise
cargo gamma run --ops all,!stmt
```

A selector that matches nothing is an error, not a silent no-op — a suppression that quietly does
nothing is the most damaging failure a mutation tool can have, because the score stays high and
nobody finds out why.

Ask what a mutator does and how to turn it off:

```bash
cargo gamma explain relational.lt_to_le
```

### What the catalog covers

**105 mutators in 23 families.** [docs/OPERATORS.md](docs/OPERATORS.md) is the full reference, with
a table per family, every mutator's academic alias, and a note on what the catalog deliberately
omits and why. `cargo gamma list ops` prints the same thing resolved against your current selection.

Every mutator is on by default. A mutator that needs a flag before it will ever run is one nobody
runs, and a gap in a mutation score that nobody can see is worse than a mutant somebody has to spend
a minute judging, so the catalog is enabled in full and the cost of the noisier families is paid
rather than hidden.

Beyond the arithmetic, relational, logical and statement families that most tools carry, these reach
places a purely expression-level rewriter cannot:

| Family | What it asks |
|---|---|
| `match_guard` | Does anything depend on this guard being right? |
| `match_arm` | Is this arm reachable, and does anything notice when it stops matching? |
| `struct_field` | Does this field's value matter, or is the default good enough? |
| `range` | Is this bound inclusive on purpose? |
| `loop` | Does this `break` or `continue` carry the loop's meaning? |
| `option`, `result` | Is the present case distinguished from the absent one, success from failure? |
| `iter` | Does anything observe that this was ordered, deduplicated, or taken from one end? |
| `string` | Does the prefix, the case, or the trimmed end actually matter? |
| `collection` | Does every element of this literal earn its place? |
| `assign_value` | Is the value assigned here ever read in a way that would notice? |

Not everything a mutation tool might want to express can be expressed, because a mutant is run by
wrapping its site as `if guard { mutant } else { original }` and so must have the same *type* as the
code it replaces. That rules out rewriting `..` as `..=`, swapping `take` for `skip`, and giving a
function returning `impl Iterator` any return-value mutants at all.
[docs/OPERATORS.md](docs/OPERATORS.md#what-the-catalog-deliberately-omits) works through the
consequences.

A [profile](docs/PROFILES.md) groups the catalog by what a mutant disturbs, so a narrower audit is
one word rather than a list you have to maintain. `@control` is everything that changes which code
runs rather than what it computes; `@numeric` is literal replacement and expression perturbation:

```bash
cargo gamma run --ops @control --file src/dispatch.rs
```

Because everything is on, expect some mutants that no test can kill. `struct_field.omit` fires on
every literal struct, and `expr` perturbs numeric values it cannot always prove are numeric. Those
are withdrawn automatically as unviable; `--unviable` lists them if you want to see what was
discarded.

## Scoping a run

Choose which packages get mutated, and which packages' tests decide a verdict:

```bash
cargo gamma run -p ledger -p ledger-core   # mutate only these packages
cargo gamma run --test-package ledger      # only these packages' tests judge a mutant
cargo gamma run --exclude-test conformance # keep a test target out of the oracle
cargo gamma run --test-workspace           # run the whole suite for every mutant
```

By default every package is mutated, and a mutant is judged only by the test binaries that can
reach it. Narrowing the tests is the largest available speedup for a workspace whose crates are
loosely coupled, since a mutant's cost is dominated by how much of the suite it has to run.

`--test-package` works at package granularity, which is too coarse when the tests you want as an
oracle and the tests you do not share a package. `--include-test` and `--exclude-test` match cargo
*target* names with `*` and `?` globs, which is the finest granularity cargo offers: a package's
unit tests take the name of the lib or bin they live in, and each file under `tests/` is a target
named after the file. Exclusion is applied last, so `--include-test "*" --exclude-test "conformance_*"`
means what it looks like.

The usual reason to reach for this is a corpus that is not an oracle at all — conformance suites,
fuzz seeds, golden-file comparisons — sitting in the same package as the tests that are. Those
target failures say nothing about whether a mutant was noticed, and letting them convict inflates
the score.

Two things to know. A pattern matching no declared test target is an error rather than a silent
no-op, because an `--exclude-test` typo quietly widens the oracle and reads in CI exactly like a
run that went well. And exclusion changes what runs, not what compiles: the targets are still built,
so this buys accuracy rather than build time. When a run does drop targets it says so, on a line
beginning `Oracle`, since a survivor under a narrowed oracle may be a mutant the missing target
would have caught.

```bash
cargo gamma run --exclude-test "conformance_*" --exclude-test "fuzz_*"
cargo gamma run --include-test ledger        # only the ledger lib's own unit tests
```

Scope decides what gets compiled, not just what gets run: a test target that cannot reach anything
being mutated is never built. `-p ledger-core` on a sixteen-crate workspace skips the test binaries
of every crate that does not depend on `ledger-core`.

Narrowing can fail. Cargo unifies features over the packages it is asked to build, so a test target
that only compiles because some other package switches a feature on cannot be built on its own.
When that happens the run builds the whole workspace instead and says so, on a line beginning
`Scope`. Aggressive narrowing with `--test-package` is the usual trigger; if you see that line, the
scope cost more than it saved.

Restrict the population to the lines a change touches, which is what makes mutation testing
affordable on a pull request:

```bash
git diff origin/main | cargo gamma run --in-diff -
cargo gamma run --in-diff change.patch
```

Sharding is not a substitute: a shard is a slice of everything, not of what changed.

Pick up where a previous run left off. Mutants the earlier report settled — killed, timed out,
unviable or ignored — are skipped; survivors are always retried, because the tests may have grown
since:

```bash
cargo gamma run --json-report report.json --iterate report.json
```

Generate a shell completion script:

```bash
cargo gamma completions bash
```

## Suppressing mutations

Not every surviving mutant is a missing test. A mutant can be *equivalent* — a program that behaves
identically, so no test could ever tell them apart — or it can sit in code that is deliberately
untested. Both cost a reviewer attention every run until somebody records the decision.

[docs/SUPPRESSION.md](docs/SUPPRESSION.md) is the full reference: the directive grammar, the seven
places a directive may go, the rules binding a comment to the code it governs, and a table
comparing the five channels. The short version follows.

Every mutator has a stable, well-known name of the form `family.transform`. That one name is the
vocabulary for the command line, the report, and every suppression channel.

On an item, as a real attribute:

```rust
#[gamma::skip(arith, reason = "fixed-point math, checked by proptest")]
fn scaled(a: i64, b: i64) -> i64 {
    a * b / 1000
}
```

Attributes in statement and expression position are still unstable in Rust, so for finer targeting
there is a comment form that is character-for-character the attribute, with `//` in front:

```rust
// #[gamma::skip(arith)]
let total = a * scale + offset;
```

When expression attributes stabilize, deleting the two slashes turns each of these into real Rust.

`#[cfg_attr(test, mutants::skip)]` and `#[cfg_attr(…, gamma::skip(…))]` are honoured too. The
predicate is deliberately not evaluated: suppression states an intent about a site, and that intent
does not change with the build configuration.

A misspelled `gamma::` directive is a usage error rather than a silent no-op, because a directive
that reads as if it works and does nothing is the one failure mode that hides real survivors.
Directives in another tool's namespace, including `mutants::`, are left alone.

### Stating what a site's fate should be

`expect_missed` and `expect_caught` are claims about the suite rather than instructions to the
generator. The mutants they govern are still generated and still run; if the outcome disagrees with
the claim, the run reports each divergence and exits 2.

```rust
#[gamma::expect_caught(relational, reason = "the boundary here is load-bearing")]
fn within(value: u32, limit: u32) -> bool {
    value < limit
}
```

A mutant that never ran — one that failed to compile, or that was suppressed — is not judged, since
it is not evidence about the suite either way.

## Sharding

Mutation testing a large workspace exhaustively does not fit in a nightly CI budget. Split the
population into shards and run a different one each night:

```bash
cargo gamma run --shard-count 30 --shard-index 7
```

Shards are assigned by hashing each mutant's content-addressed identity, and two consequences matter
in practice. A mutant keeps its shard as the code around it changes, so coverage accumulates across
nights instead of resetting whenever somebody edits a file. And raising the shard count moves only
the mutants that have to move, rather than reshuffling everything and throwing away the rotation you
had already paid for.

Keep each night's report and merge the rotation to get a score for the whole population:

```bash
cargo gamma run --shard-count 30 --shard-index $((10#$(date +%j) % 30)) \
    --json-report reports/$(date +%F).json
cargo gamma merge reports --window 45 --min-score 70 --html merged.html
```

Merging unions verdicts by mutant identity and keeps the most recent one. Because identity is
content-addressed, a mutant whose code has since been edited is not credited with the verdict its
predecessor earned — it reappears as never tested, which is also how it stays out of the
denominator.

Removing the *old* identity needs one more thing, because a union by itself never drops anything: at
least one input has to be an unsharded run or listing, which states the complete population of every
file it covers. An identity absent from the newest such input has been withdrawn, and the summary
counts it under `Withdrawn`. A sharded report describes only its own slice, so it never withdraws
anything — merge a full `list mutants --json` alongside the rotation to keep the denominator honest:

```bash
cargo gamma list mutants --json-report reports/current.json
cargo gamma merge reports --window 45
```

The summary also reports how fresh the verdicts are and which shards the rotation has yet to visit.

## Controlling a run

```bash
cargo gamma run --jobs 8                  # mutants tested in parallel
cargo gamma run --timeout 30              # seconds per mutant, instead of a multiple of the baseline
cargo gamma run --minimum-test-timeout 5  # floor under the computed budget
cargo gamma run --build-timeout 600       # bound the single build
cargo gamma run --min-score 80            # fail the run below a score
cargo gamma run --dry-run                 # report the plan without building anything
cargo gamma run --caught                  # list what the suite killed, not just what escaped
cargo gamma run --unviable                # list the mutants that could not compile
cargo gamma run --leak-dirs               # keep the scratch tree, and say where it is
cargo gamma run --scratch-dir /fast/disk  # put the copy and its artifacts somewhere else
```

A run computes each mutant's budget from the unmutated suite, so a fast suite gets a tight one.
`--minimum-test-timeout` stops a loaded machine from reporting scheduling noise as a hang. The build
is paid for exactly once, so a build that never finishes costs the whole run; `--build-timeout` and
`--build-timeout-multiplier` bound it, and a build that outstays its budget is stopped rather than
merely complained about afterwards.

The workspace is copied to `target/gamma` before anything is rewritten, and build artifacts stay
there between runs so repeated runs compile incrementally. `--scratch-dir` moves the lot: it lets a
read-only checkout be mutated, gets the copy off a slow or network filesystem, and gives concurrent
runs somewhere separate to work. Two runs sharing one scratch directory is refused rather than
allowed to corrupt both, so a second run needs its own directory.

Control how the tree is compiled and how the tests are invoked:

```bash
cargo gamma run --all-features
cargo gamma run --features serde,rayon --no-default-features
cargo gamma run --profile release         # one build, thousands of mutants: often worth it
cargo gamma run --cargo-arg --offline
cargo gamma run --cargo-test-arg --skip --cargo-test-arg slow_
cargo gamma run -- --skip slow_           # the same thing, for arguments that need no escaping
```

Feature selection reaches discovery as well as the build, so the mutants found and the tree compiled
agree about which code exists.

Point the run somewhere other than the current directory, or say explicitly that the whole workspace
is in scope:

```bash
cargo gamma run --dir ../ledger    # analyze a workspace elsewhere
cargo gamma run --workspace        # every package: accepted for symmetry with cargo, already the default
```

Reach an error type that does not implement `Default`:

```bash
cargo gamma run --error 'MyError::Io' --error 'MyError::Eof'
```

Each value becomes its own `fn_value.err_with` mutant on every function returning a `Result`.

By default each mutant gets a budget derived from how long the unmutated suite took, because a
mutant that turns a loop bound into an infinite loop should be cut off in seconds rather than
whenever a fixed global timeout happens to expire. `--timeout` replaces that measurement with a
fixed number of seconds, and `--timeout-multiplier` keeps the measurement but changes how much
slower than the baseline a mutant may be before it is called a timeout. Prefer the multiplier: a
fixed timeout that was generous on a developer's laptop is not generous on a loaded CI runner.

Some mutants cannot compile — replacing a body with `Some(Default::default())` only works when the
type implements `Default`. These are withdrawn automatically, rebuilt without, and reported as
unviable rather than counted against the score. Withdrawal is iterative, because rustc reports only
the errors it reaches before it gives up, so a large tree can need several rounds to converge.
`--rollback-rounds` raises the cap; raise it when a run stops with a rollback-limit error and the
withdrawal counts it is printing are still falling.

Two flags control what the terminal sees. Both take `auto`, `always` or `never`, and both default to
`auto`, which means "on when standard error is a terminal":

```bash
cargo gamma run --color never --progress never   # what a CI log wants
```

`--progress` governs the live counter, which is redrawn in place and therefore turns a CI log into
thousands of near-identical lines when nothing is there to interpret the escapes. `--color` governs
styling alone, so a log can stay colourless without losing the progress the flag would otherwise
suppress.

Every test process a run launches, baseline included, has `CARGO_GAMMA=1` set. A suite that drives
cargo itself needs this: a nested build inside the scratch tree fails for reasons unrelated to any
mutant, and that shows up as a red baseline before a single mutant runs.

## Reports

```bash
cargo gamma run --html report.html --json-report report.json
```

The JSON is the [`mutation-testing-elements`](https://github.com/stryker-mutator/mutation-testing-elements)
interchange format, which is what the Azure DevOps and GitHub mutation report extensions consume, so
no translation step is needed.

The HTML is a single self-contained file: the viewer and the results are both embedded, so it opens
from a CI artifact, a file share, or a machine with no network at all. Pass `--html-external` to load
the viewer from a CDN instead, which produces a much smaller file that needs network access to read.

## Continuous integration

A mutation report that lives in an artifact zip is a report nobody reads, so the findings are
delivered where the reviewer already is.

```bash
cargo gamma run --sarif mutants.sarif        # then upload with github/codeql-action/upload-sarif
```

Inside GitHub Actions, no flag is needed at all: `--annotations` defaults to `auto`, which detects
the runner and then writes surviving mutants to the diff as workflow annotations and a score table
to the job summary. `--annotations none` turns it off, `--annotations github` forces it on.

All three surfaces publish **survivors only**. A killed mutant is the tool working, and reporting it
would bury the signal under its own success. Uncovered mutants are included, because "no test
reaches this" is a stronger finding than "a test reached it and said nothing".

Both surfaces are capped at what GitHub actually accepts: ten annotations per step, because that is
all GitHub keeps of a level and printing more produces a log full of commands that had no effect,
and five thousand SARIF results within ten megabytes, because a larger upload is rejected whole
rather than trimmed. When a cap bites, the run says so and the full population stays in the report.

SARIF rule identifiers are the stable mutator names, so GitHub's grouping and dismissal work per
operator: a team can permanently dismiss every `literal.int_zero` alert without touching anything
else, and that decision keeps applying to code written next year. Results are fingerprinted by the
content-addressed mutant ID, so an alert follows its code through reformatting instead of being
dismissed and resurrected. The level is `note` by default, because a surviving mutant is an
observation about the test suite rather than a defect in the code, and drowning the security tab is
how a good signal gets turned off; `--sarif-level warning` raises it.

### Exit codes

| Code | Meaning |
| ---- | ------- |
| `0` | The run finished and every gate that was configured passed. |
| `1` | Usage error: an argument, a filter pattern or a configuration key was wrong. |
| `2` | The run finished and a gate failed — the score was below `--min-score`, or a `gamma::expect` directive was contradicted. |
| `3` | The run could not proceed: the baseline failed, the tree would not compile, or the scratch directory was already in use. |
| `70` | An internal error. This is a bug; the message says what to report. |

**Surviving mutants do not fail the process on their own.** A run with survivors and no gate exits
`0`, deliberately: adopting mutation testing on an existing codebase starts with survivors, and a
tool that fails the build on the first day is a tool that gets removed on the second. A CI job that
wants survivors to be fatal says so:

```bash
cargo gamma run --min-score 100          # any survivor fails the job
cargo gamma run --min-score 80           # a ratchet you can raise over time
```

`--min-score` is the ratchet worth reaching for. Setting it to the score you have today makes the
number impossible to lose ground on, and raising it is a one-line change with a visible owner.

## Configuration

Settings that a project has agreed on belong in `.cargo/gamma.toml` rather than in every CI job:

```toml
ops           = ["@arithmetic", "@relational", "stmt"]
exclude-files = ["src/generated/**"]
exclude-tests = ["conformance_*"]
min-score     = 70.0

[shard]
count = 30

[reporters]
html  = "target/mutation-report.html"
sarif = "target/mutants.sarif"
```

An unknown key is an error rather than a setting that quietly does nothing. Scalars given on the
command line win; lists concatenate, so adding one exclusion on the command line does not silently
drop the ones the project agreed on.

[docs/CONFIGURATION.md](docs/CONFIGURATION.md) documents every key, the flag it corresponds to, and
what it costs to get it wrong.

## Fixing timeouts and unviable mutants

Some sites cannot usefully be mutated — a hand-written spin loop, a driver poll, a reactor. `fix`
runs the suite and writes the suppression for you:

```bash
cargo gamma suppress --dry-run-suppress            # print the diff, change nothing
cargo gamma suppress                               # write directives for timeouts
cargo gamma suppress --eligible timeout,unviable   # include mutants that would not compile
```

**A surviving mutant is never eligible, and cannot be made eligible.** A survivor is a real gap in
the test suite; suppressing it would remove the gap from the score rather than from the code, and
the moment that is possible every score the tool reports becomes unfalsifiable.

Generated directives name the exact mutators that tripped — never a family, never `all` — and carry
a tag, a reason and the date, so they can be audited later. After writing, discovery runs again and
the suppressed set is compared in both directions: every intended mutant must now be suppressed, and
nothing else may have become suppressed. If either check fails, every edit is reverted.

## Coming from cargo-mutants

`#[mutants::skip]` keeps working, so no source has to change. `.cargo/mutants.toml` is deliberately
**not** read — it is a different schema for a different tool, and honouring it silently would let
another tool's settings quietly change which mutants are skipped here. Translate it once instead:

```bash
cargo gamma migrate --dry-run     # print the proposed gamma.toml
cargo gamma migrate               # write it, leaving mutants.toml in place
```

`--config <FILE>` points at a configuration file elsewhere and `--no-config` runs with none, which
is what makes a run reproducible from a script regardless of what is checked in.

Nothing is dropped: a key with no equivalent becomes a `TODO` comment carrying the original text, and
every translated line names the key it came from.

Command lines are translated too, which turns a CI workflow into a one-liner:

```bash
$ cargo gamma migrate --command cargo mutants --shard 3/8 -j 4
cargo gamma run --shard-index 3 --shard-count 8 -j 4
```

### Comparing the numbers

The catalogs differ, so the scores will too: this tool generates a much larger population, and a
larger population almost always means a lower score for the same suite. To compare like with like,
run the [`@parity` profile](docs/PROFILES.md), which is exactly the operator set cargo-mutants
generates — replacing what a function returns, and nothing else:

```bash
cargo gamma run --ops @parity
```

Once the two agree, drop the flag and let the rest of the catalog tell you what the smaller
population was not asking.

### Known gaps

Doctests are not built or run, so a mutant whose only coverage is a doctest is reported as missed.

Types are read syntactically rather than resolved, so a type alias is not seen through and falls
back to `Default::default()`. Where the tool can tell that no such guess could hold — a bare type
parameter, an associated type projected out of one, an `impl Trait`, a `Box<dyn Trait>` — it
withholds the mutant instead of generating one that cannot compile. The same absence of type
resolution is why `expr` occasionally perturbs an expression that turns out not to be numeric.

Match arms, match guards, struct fields and recursive return values used to be on this list. They
are covered now, by the `match_arm`, `match_guard` and `struct_field` families and by the recursive
return-value synthesis described in [docs/OPERATORS.md](docs/OPERATORS.md).

## Hangs

Deleting a statement or relaxing a loop condition makes runaway loops common, and a hung mutant is
the most expensive verdict a run can produce — it is the only one whose cost is decided by how long
you are willing to wait. Waiting out a timeout derived from the whole suite means spending two
minutes to learn that a one-line change made a twelve-millisecond test spin forever.

So the run does not wait. The baseline measures the longest a healthy suite legitimately goes
without saying anything, and a mutant that goes quiet for much longer than that is presumed hung and
cut off. The budget is calibrated rather than fixed, because a suite whose slowest test takes half a
minute goes half a minute quiet when it is perfectly healthy, and a constant would either accuse it
or be too loose to help a suite of millisecond unit tests.

```
     TIMEOUT src/parse.rs:88:11: replace remaining > 0 with (remaining) >= (0) [relational.gt_to_ge]: stalled, last test named was `tests::round_trip`
       Hangs a mutant is cut off after 5.0s of silence; the baseline'"'"'s longest was 0.4s
```

The report names the last test the harness announced, which is a landmark rather than a diagnosis:
libtest runs tests in parallel and names each one only once it has finished, so the test that is
actually spinning is usually one it has not got round to naming. Re-running the binary with
`--test-args --test-threads=1` makes the name exact, at the cost of the parallelism. A timeout counts as detected — a
hang is a behavior change the suite noticed, just expensively — so hung mutants are listed
explicitly rather than disappearing into the caught total. `cargo gamma suppress --eligible timeout`
writes suppressions for them, and `--no-stall-detection` restores waiting out the full budget.

### Bounding what a mutant allocates

A mutation can turn bounded allocation into unbounded allocation — a loop bound inverted, a
capacity computed by multiplication instead of division. The timeout does eventually stop such a
mutant, but only after it has taken the machine into swap, and possibly after the kernel has killed
something that had nothing to do with the run.

`--memory enforce` is the default, on the same reasoning as the timeout: the user who most needs
protecting from a runaway allocation is the one who never thought to ask for it. Each test binary's
whole process tree is metered during the baseline, and every mutant of that binary is then held to a
ceiling derived from what it measured — by default the larger of twice the baseline peak and the
baseline peak plus 128 MiB. The multiplier governs large suites; the headroom governs small ones,
where doubling a few megabytes would leave no room for a lazily initialized table.

A mutant the kernel stops for reaching its ceiling is reported as `OUTOFMEM` and counted in its own
column of the summary, with a note naming the binary, the peak it reached and the ceiling it passed:

```
OUTOFMEM src/buffer.rs:5:15: replace steps.min(4) with steps.max(4) [iter.min_to_max]:
         `subject-2181f69f` reached 192.6 MB, past the 192.6 MB this run allowed it

Summary 1 mutant (0 caught, 0 missed, 0 timed out, 1 out of memory, 0 uncovered => 100.0%)
```

It counts as detected, like a timeout: the baseline established that this workload fits under this
ceiling *without* the mutant, so the mutant is what changed. It gets its own outcome rather than
being folded into `caught` because the suite's assertions did not fail — the kernel stopped the
workload — and a reader who cannot tell those apart goes looking for a failing test that does not
exist. It is also the outcome most likely to be wrong: a ceiling set too tight convicts a healthy
mutant, which is why the note carries both numbers.

`--memory measure` meters and reports without ever stopping a mutant, which is what to use if you
want the numbers before you trust the ceiling. `--memory off` disables both.

Enforcement needs real support from the host: a delegated cgroup v2 on Linux, or a job object on
Windows. Nothing else accounts for a whole process tree, which is what a test binary is once it
starts a server or a nested build. On Linux, delegation is usually what needs arranging:

```bash
systemd-run --user --scope -p Delegate=yes cargo gamma run
```

What happens when the host cannot provide it depends on who asked. A run that merely inherited the
default continues unbounded and says so once, on the diagnostic stream:

```
Memory what a mutant allocates is not bounded on this host: ... needs one delegated to this process
```

A run that named `--memory` or a size flag stops instead, because someone who asked for a guarantee
is worse off believing they have one. The same split applies to `--no-baseline`, which leaves no
measurement to derive a ceiling from: the default degrades, an explicit request is an error.

macOS and other platforms report enforcement as unsupported rather than offering a weaker limit
under the same name. An inherited `RLIMIT_AS` is not an equivalent — it bounds each process
separately, and bounds reserved address space rather than resident memory.

| Flag | Meaning |
| --- | --- |
| `--memory <off\|measure\|enforce>` | How much memory control to place around each test binary (default `enforce`) |
| `--memory-multiplier <FACTOR>` | Multiple of the baseline peak a mutant may reach (default 2) |
| `--memory-headroom <SIZE>` | Absolute headroom over the baseline peak (default 128MiB) |
| `--memory-limit <SIZE>` | An explicit ceiling instead of a derived one; implies `--memory enforce` |
| `--baseline-memory-limit <SIZE>` | A ceiling for the baseline runs themselves; implies `--memory measure` |

## Diagnosing a slow run

Mutation testing is the kind of tool that gets adopted enthusiastically, runs for four hours, and is
then quietly deleted from the CI configuration. Two options exist to prevent that.

```bash
cargo gamma run --estimate            # project the run once the fixed cost is measured, then carry on
cargo gamma run --advice advice.md    # run, then write down where the time went
```

`--estimate` reports at the exact point a run stops measuring and starts waiting. Everything behind
it was measured rather than guessed — the build really built, the baseline really ran, and mutants
that cannot compile were really withdrawn — so the only uncertainty left is how much of the suite a
killed mutant reaches. It prints one line and then continues, because stopping there would throw
away the build it just paid for:

```
    Estimate 14m to 31m for 18 751 mutants at 16 jobs, 2.4h worst case
```

`--advice` turns a finished run into a Markdown document: a list of findings, each a measured
symptom, a named cause, a remedy, and — never omitted — what the remedy costs in signal. It closes
with the per-family cost and survivor table the low-yield finding is drawn from.

```markdown
### crates/parser/src/tables.rs alone is 34% of the population (1 204 mutants)

- 11m of CPU time, 2 survivors found there

**Remedy.** If it is generated, tabular or macro-expanded code, exclude it with `--exclude-file` or
the `exclude-files` config key. If it is hand-written, this is not a problem — it is where the logic
is.

**Costs.** Exactly 1204 mutants stop being tested, 2 of which are currently finding gaps in the
suite.

### Yield by family

| Family | Mutants | CPU | Survivors | Survivors/CPU-h |
|---|---:|---:|---:|---:|
| `relational` | 4 210 | 22m | 61 | 166.4 |
| `arith` | 1 980 | 11m | 9 | 49.1 |
```

Every mitigation available here trades information for time. A recommendation that hides the trade
is worse than no recommendation, because it will be taken.

The same diagnosis is appended to the GitHub Actions job summary whenever `--annotations` is
active, so the panel a team reads every morning carries not just the score but what to do about it.

## What a mutant is not run against

Rust cannot call code it does not link. A test binary built from a package that does not depend,
directly or transitively, on the package a mutant lives in can never execute that mutant, so
running it there costs time and can only produce the answer it already had. Every such pairing is
skipped, and the binaries that remain are run cheapest-first, so a mutant that any test kills is
usually killed by the fastest binary that could have killed it.

The filter is built from declared dependencies rather than a resolved graph, which makes it free,
and it fails open: a package either side of the relation that cannot be identified is assumed
reachable. A missed skip costs a little time; a wrong skip would hide a real gap in the suite.

A mutant that no test binary can reach is reported `uncovered` rather than `survived`. Both count
against the score in the same way, but they call for different responses — a survivor means a test
ran and did not notice, while an uncovered mutant means no test exists at all.

### Conditional compilation

Mutants are found by reading source, so they are found in code the build is not going to compile —
a module behind a `#[cfg(feature = ...)]` whose feature is off, or one behind `#[cfg(windows)]` on
Linux. Such a mutant cannot change what any test observes, so it would otherwise survive every run
and be reported as a gap in a suite that never had a chance.

After the build, the run asks the compiler which files it actually read, and any mutant outside that
set is taken out of the run and counted as `not built` on the summary line instead of being scored.
Deferring to the compiler rather than re-evaluating `#[cfg]` predicates means this covers features,
target platform and everything else cargo and rustc already agreed on, with no second opinion to
disagree with the first.

The remedy is a feature flag: `--all-features`, or `--features` naming the ones that matter. A run
that excludes anything says so, because a population smaller than the one `gamma list` reports is
otherwise unexplained.

One limit is worth knowing: this works per file. A `#[cfg]` on a single function inside a file that
was compiled is not detected, and a mutant there is still reported as a survivor.

### Doctests

Doctests are not built, run, timed or reported. `cargo test --doc` is never invoked, so a mutant
whose only coverage is an example in a doc comment is reported `uncovered`, and one that a doctest
would have caught is reported `survived`.

This is a deliberate cost decision rather than an oversight. The schema exists so that a mutant
costs one test-process launch instead of one rebuild; `rustc` compiles and links a separate binary
per doctest, so admitting them would reintroduce a per-mutant compile for exactly the tests that
tend to assert least about behaviour. A crate whose real coverage lives in its doc examples is
better served by promoting those examples into `#[test]` functions, which makes them faster for
`cargo test` as well.

The consequence to keep in mind when reading a report: on a crate that leans on doctests, the score
is a lower bound, and the `uncovered` count is the place the difference shows up.

## Status

Working, and not yet complete. The mutant model, the operator catalog, source analysis,
suppression, sharding, the command-line surface, the CI surfacing, execution — one build, baseline, parallel
per-mutant runs, package-reachability filtering, timeouts and automatic withdrawal of unviable
mutants — the JSON and HTML reports,
the configuration file, `suppress`, `migrate` and `merge` are implemented and tested.

Not yet built: per-test coverage mapping, and the deterministic tick budget that depends on it —
hangs are currently detected from output silence, which needs no instrumentation but is not
byte-identical across machines the way a tick count would be.

## Contributing

Contributions are welcome. Please file issues or open pull requests.

## License

Licensed under the [MIT License](LICENSE).
