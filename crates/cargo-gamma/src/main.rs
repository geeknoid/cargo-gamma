//! A cargo tool for fast mutation testing.
//!
//! Mutation testing answers the question coverage cannot: not "did a test execute this line?" but
//! "would a test have noticed if this line were wrong?" It answers it by changing the code in small
//! deliberate ways and checking that the suite fails. A change the suite does not notice is a gap.
//!
//! The reason it is not universal is cost: the conventional implementation rebuilds the crate once
//! per mutant. `cargo-gamma` builds once, compiling every mutant into the same binaries behind a
//! runtime guard and selecting one per process, so testing a mutant costs a process launch instead
//! of a build.
//!
//! # Quick start
//!
//! ```text
//! cargo install cargo-gamma
//! cd my-workspace
//! cargo gamma
//! ```
//!
//! `cargo gamma` with no subcommand means [`run`](#run). It builds the workspace once, measures an
//! unmutated baseline, then tests every mutant in parallel and prints the ones the suite missed:
//!
//! ```text
//!       MISSED src/parse.rs:88:21: replace < with <= [relational.lt_to_le]
//!       MISSED src/cache.rs:41:9: delete self.hits += 1 [stmt.delete_assign]
//!      TIMEOUT src/scan.rs:12:16: replace + with - [arith.add_to_sub]
//!        Found 303 mutants in 41 files
//!      Summary 303 mutants (294 caught, 7 missed, 2 timed out, 0 out of memory, 0 uncovered => 97.7%)
//! ```
//!
//! Every line names the mutator that produced it in brackets. That name is the whole vocabulary:
//! it is what you pass to `--ops`, what you write in a suppression, what `explain` describes, and
//! what the reports group by.
//!
//! Before committing to a long run, ask what it will cost. `--estimate` stops at the point the run
//! would stop measuring and start waiting, and projects the rest from the mutants it has already
//! tested:
//!
//! ```text
//! cargo gamma run --estimate
//! ```
//!
//! # The command surface
//!
//! ```text
//! cargo gamma run          # the default: build once, test every selected mutant
//! cargo gamma list         # what would be done, without doing it
//! cargo gamma explain      # describe a mutator, a family, a profile, or a mutant id
//! cargo gamma suppress     # write suppressions for mutants that cannot usefully be tested
//! cargo gamma merge        # combine per-shard reports into one score
//! cargo gamma migrate      # translate a cargo-mutants project into this one's vocabulary
//! cargo gamma completions  # print a shell completion script
//! ```
//!
//! ## run
//!
//! The full loop. It copies the workspace to a scratch tree, rewrites the selected sources so that
//! every mutant is a guarded branch, builds that tree once, times an unmutated baseline to size the
//! per-mutant timeout, and then runs each mutant in its own process.
//!
//! ```text
//! cargo gamma run                          # everything, default operator profile
//! cargo gamma run --dry-run                # find and report mutants, build nothing
//! cargo gamma run --estimate               # measure a little, project the rest
//! cargo gamma run -v                       # also list the mutants the suite caught
//! cargo gamma run -- --test-threads=1      # arguments after `--` go to every test binary
//! ```
//!
//! ## list
//!
//! Answers "what is in scope?" without building anything, which makes it the cheapest way to check
//! that a selector or a glob means what you thought.
//!
//! ```text
//! cargo gamma list             # every mutant that would be tested
//! cargo gamma list ops         # the mutator catalog, marking the ones currently enabled
//! cargo gamma list files       # the source files that would be mutated
//! cargo gamma list --json      # the same, machine-readable
//! ```
//!
//! `list` accepts the same scoping options as `run`, so it answers the question for the exact
//! selection you are about to run:
//!
//! ```text
//! cargo gamma list --ops '@arithmetic' --file 'src/money/**'
//! ```
//!
//! ## explain
//!
//! Turns a name from a report back into prose — a mutator, a family, a profile, an academic alias,
//! or the content-addressed id of a specific mutant.
//!
//! ```text
//! cargo gamma explain relational.lt_to_le
//! cargo gamma explain arith
//! cargo gamma explain '@removal'
//! cargo gamma explain ROR
//! ```
//!
//! ## suppress
//!
//! Some sites cannot usefully be mutated: a hand-written spin loop, a driver poll, a reactor whose
//! mutant simply never returns. `suppress` runs the suite, finds those sites, and writes the
//! suppression comment into the source for you, so the next run does not pay for them again.
//!
//! ```text
//! cargo gamma suppress --dry-run     # show the edits without making them
//! cargo gamma suppress               # make them
//! ```
//!
//! ## merge
//!
//! Combines the reports of a shard rotation into one answer. See [Sharding](#sharding).
//!
//! ```text
//! cargo gamma merge reports --window 45 --min-score 70 --html merged.html
//! ```
//!
//! ## migrate
//!
//! Translates a `cargo-mutants` configuration file, or a `cargo mutants` command line, into this
//! tool's vocabulary. `#[mutants::skip]` is honored natively, so existing suppressions keep working
//! whether or not you migrate them.
//!
//! ```text
//! cargo gamma migrate --dry-run
//! cargo gamma migrate --command -- --exclude 'src/generated/**' --timeout 60
//! ```
//!
//! # The mutator catalog
//!
//! Every mutator has a stable, well-known name of the form `family.transform`. The families:
//!
//! | Family | What it changes | Example |
//! |---|---|---|
//! | `fn_value` | a whole function body, replaced by a plausible value | `-> u32` becomes `{ 0 }` |
//! | `relational` | comparison operators | `<` becomes `<=` |
//! | `arith` | binary arithmetic | `+` becomes `-` |
//! | `bitwise` | bitwise operators | `&` becomes <code>&#124;</code> |
//! | `shift` | shift operators | `<<` becomes `>>` |
//! | `assign` | compound assignment | `+=` becomes `-=` |
//! | `logical` | short-circuiting operators | `&&` becomes <code>&#124;&#124;</code> |
//! | `cond` | branch conditions | `if c` becomes `if !c` |
//! | `unary` | unary operators, removed | `-x` becomes `x` |
//! | `literal` | literal constants | `8` becomes `0`, `"msg"` becomes `""` |
//! | `stmt` | whole statements, deleted | `self.hits += 1;` is removed |
//!
//! `fn_value` is the family `cargo-mutants` implements; the other ten are what this tool adds.
//! `cargo gamma list ops` prints all of them with a marker beside the ones the current selection
//! enables, and `cargo gamma explain <name>` describes any one of them.
//!
//! ## Selecting mutators
//!
//! `--ops` takes a comma-separated selector list. A selector is a full mutator name, a family
//! prefix, an `@profile`, an academic alias, or `all`; `!` in front removes rather than adds, and
//! selectors apply left to right, so a list reads as a sentence.
//!
//! ```text
//! cargo gamma run --ops relational.lt_to_le      # one mutator
//! cargo gamma run --ops arith,bitwise            # two families
//! cargo gamma run --ops '@arithmetic'            # a profile
//! cargo gamma run --ops ROR                      # an academic alias
//! cargo gamma run --ops 'all,!stmt'              # everything except statement deletion
//! cargo gamma run --ops '@default,literal'       # the default set, plus one more family
//! ```
//!
//! An unknown selector is an error with a spelling suggestion, never a silently empty selection.
//!
//! The profiles:
//!
//! | Profile | Contents |
//! |---|---|
//! | `@all` | every registered mutator |
//! | `@default` | the mutators enabled when none are named |
//! | `@parity` | the `cargo-mutants` operator set |
//! | `@boundary` | relational and boundary conditions |
//! | `@arithmetic` | arithmetic, bitwise, shift and compound assignment |
//! | `@logical` | logical operators and branch conditions |
//! | `@removal` | statement and side-effect deletion |
//! | `@literals` | literal and constant replacement |
//! | `@extreme` | everything, including mutators that are noisy by default |
//!
//! The aliases are the names the mutation-testing literature uses — `ROR` for relational operator
//! replacement, `AOR` for arithmetic, `LCR` for logical connectors, `COR` for conditionals, `UOI`
//! for unary operator insertion, `CRP` for constant replacement, `ASR` for assignment replacement,
//! `SDL` for statement deletion — and they resolve case-insensitively.
//!
//! # Scoping a run
//!
//! Three axes narrow what gets mutated: packages, files, and lines.
//!
//! ```text
//! cargo gamma run -p my-core                       # one package
//! cargo gamma run --workspace                      # every package (the default)
//! cargo gamma run --file 'src/parser/**'           # only these files
//! cargo gamma run --exclude-file 'src/generated/**'
//! ```
//!
//! `--in-diff` restricts mutation to lines a unified diff added or changed, which turns a
//! whole-workspace tool into a per-pull-request one:
//!
//! ```text
//! git diff origin/main... | cargo gamma run --in-diff -
//! ```
//!
//! Scoping *what is mutated* is separate from scoping *what is run to decide the verdict*.
//! `--test-package` and `--test-workspace` control the latter:
//!
//! ```text
//! cargo gamma run -p my-core --test-package my-core   # only my-core's tests judge the mutants
//! cargo gamma run -p my-core --test-workspace         # every test in the workspace judges them
//! ```
//!
//! Independently of those flags, a test binary that cannot reach a mutant is never run against it.
//! Rust cannot call code it does not link, so a binary from a package that does not depend on the
//! mutant's package can only produce the answer it already had. A mutant that *no* binary can reach
//! is reported `uncovered` rather than `survived`: both cost score, but a survivor means a test ran
//! and did not notice, while an uncovered mutant means no test exists at all.
//!
//! # Suppressing mutations
//!
//! Some mutants are not worth testing, and saying so should be surgical, reviewable, and checked.
//! There are four channels, all speaking the same selector vocabulary.
//!
//! **An attribute**, on any item, validated by the compiler:
//!
//! ```rust,ignore
//! #[gamma::skip(arith, reason = "fixed-point math, checked by proptest")]
//! fn scaled(a: i64, b: i64) -> i64 {
//!     a * b / 1000
//! }
//! ```
//!
//! **A comment**, character-for-character the attribute with `//` in front, which is what reaches
//! statements and expressions while attributes in those positions are still unstable:
//!
//! ```rust,ignore
//! // #[gamma::skip(arith)]
//! let total = a * scale + offset;
//! ```
//!
//! When expression attributes stabilize, deleting the two slashes turns every such comment into
//! real Rust. A misspelled selector in a comment is diagnosed by the tool rather than ignored.
//!
//! **A config rule**, for a policy the whole project agreed on — see [Configuration](#configuration).
//!
//! **The `suppress` command**, which finds the sites empirically and writes the comments for you.
//!
//! Two attributes assert the opposite of skipping, and fail the run when the assertion breaks:
//!
//! ```rust,ignore
//! // A known gap, recorded so that closing it is deliberate rather than accidental.
//! #[gamma::expect_missed(literal, reason = "log text is not asserted on")]
//! fn describe(n: usize) -> String {
//!     format!("processed {n} items")
//! }
//!
//! // A test whose value someone wants protected from erosion.
//! #[gamma::expect_caught]
//! fn checksum(bytes: &[u8]) -> u32 {
//!     bytes.iter().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(u32::from(*b)))
//! }
//! ```
//!
//! # Sharding
//!
//! Exhaustive mutation testing of a large workspace does not fit in a nightly CI budget. Split the
//! population into shards and run a different one each night:
//!
//! ```text
//! cargo gamma run --shard-count 30 --shard-index 7
//! ```
//!
//! Shards are assigned by jump consistent hashing over each mutant's content-addressed identity.
//! Two consequences matter in practice: a mutant keeps its shard as the code around it changes, so
//! coverage accumulates instead of resetting; and raising the shard count moves only the mutants
//! that have to move, rather than reshuffling everything the way `hash % count` would.
//!
//! Keep each night's report and merge the rotation for a score covering the whole population:
//!
//! ```text
//! cargo gamma run --shard-count 30 --shard-index $((10#$(date +%j) % 30)) \
//!     --json-report reports/$(date +%F).json
//!
//! cargo gamma merge reports --window 45 --min-score 70 --html merged.html
//! ```
//!
//! Merging unions verdicts by mutant identity and keeps the most recent. Because identity is
//! content-addressed, a mutant whose code has since been edited is not credited with the verdict
//! its predecessor earned — it reappears as never tested, which is also how it stays out of the
//! denominator. The merge summary reports how fresh the verdicts are and which shards the rotation
//! has yet to visit.
//!
//! # Reports
//!
//! ```text
//! cargo gamma run --json-report target/mutants.json   # mutation-testing-elements schema
//! cargo gamma run --html target/mutants.html          # self-contained interactive page
//! cargo gamma run --sarif target/mutants.sarif        # SARIF 2.1.0, for code scanning
//! cargo gamma run --advice target/advice.md           # where the time went, and what to do
//! ```
//!
//! The HTML report embeds the viewer and the source, so it opens on a machine with no network at
//! all; `--html-external` loads the viewer from a CDN instead, for a smaller file. The JSON report
//! is the interchange format the whole `mutation-testing-elements` ecosystem reads, and it is also
//! what `merge` and `--iterate` consume.
//!
//! `--advice` writes a Markdown diagnosis: which files and mutator families dominate the run, what
//! each family actually bought in findings, and what trimming it would cost in signal.
//!
//! # Continuous integration
//!
//! ```text
//! cargo gamma run --min-score 80 --annotations github
//! ```
//!
//! `--min-score` fails the run when the score falls below a percentage, which is what turns the
//! score into a ratchet. `--annotations` places each surviving mutant on the diff as a review
//! annotation and writes a job summary, so the result is read where the change is, not in a log.
//!
//! Exit codes are distinct so a job can tell "the suite has a gap" from "the tool could not run":
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | success |
//! | 1 | usage error — a bad option, selector, or glob |
//! | 2 | the run completed but a gate failed, such as `--min-score` |
//! | 3 | the run could not proceed — the build failed, the baseline failed, no mutants |
//! | 70 | an internal error |
//!
//! For a pull-request job, combine `--in-diff` with annotations so only the change under review is
//! mutated and the findings land on it:
//!
//! ```text
//! git diff "origin/$BASE"... | cargo gamma run --in-diff - --annotations github --min-score 75
//! ```
//!
//! `--iterate` skips mutants an earlier JSON report already resolved, which makes a second run
//! after a fix cost only the mutants that were still open:
//!
//! ```text
//! cargo gamma run --iterate target/mutants.json
//! ```
//!
//! # Configuration
//!
//! Settings a project has agreed on belong in `.cargo/gamma.toml` rather than in every CI job:
//!
//! ```toml
//! ops           = ["@arithmetic", "@boundary", "stmt"]
//! exclude-files = ["src/generated/**"]
//! min-score     = 70.0
//!
//! [shard]
//! count = 30
//!
//! [reporters]
//! html  = "target/mutation-report.html"
//! sarif = "target/mutants.sarif"
//! ```
//!
//! An unknown key is an error rather than a setting that quietly does nothing. Scalars given on the
//! command line win; lists concatenate, so adding one exclusion on the command line does not
//! silently drop the ones the project agreed on. `--config <FILE>` reads a different file and
//! `--no-config` ignores the file entirely, which is how a one-off run escapes the project policy.
//!
//! # Tuning a run
//!
//! ```text
//! cargo gamma run --jobs 8                     # mutants tested in parallel
//! cargo gamma run --timeout 60                 # absolute per-mutant budget, in seconds
//! cargo gamma run --timeout-multiplier 3       # or: a multiple of the measured baseline
//! cargo gamma run --minimum-test-timeout 20    # a floor, however fast the baseline was
//! cargo gamma run --build-timeout 900          # abandon a build that will not finish
//! cargo gamma run --profile release            # build with a different cargo profile
//! cargo gamma run --features slow-path         # cargo feature selection, as usual
//! cargo gamma run --scratch-dir /fast/disk     # put the scratch tree somewhere quicker
//! ```
//!
//! A mutant that stops producing output is cut off before its whole budget elapses, which is what
//! keeps one infinite loop from costing the run a full timeout; `--no-stall-detection` waits the
//! budget out instead. `--leak-dirs` keeps the scratch tree and says where it is, which is how you
//! reproduce a build failure by hand.
//!
//! Mutants that do not compile — a type that has no `Default`, an operator the types do not
//! support — are withdrawn automatically over successive build rounds and reported as `unviable`
//! rather than counted against the score. `--unviable` lists them individually.
//!
//! # How it works
//!
//! Conventional mutation testing rebuilds the crate under test once per mutant. A workspace with
//! ten thousand mutants and a ninety-second build spends ten days building and a few minutes
//! testing.
//!
//! `cargo-gamma` builds **once**. Every selected mutant is compiled into the same set of test
//! binaries behind a *guard* — a branch taken only when that mutant's ordinal matches the one named
//! by the `GAMMA_ACTIVE` environment variable:
//!
//! ```text
//! original:     a < b
//! instrumented: (if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })
//! ```
//!
//! This is the *mutant schema*, after Untch, Offutt and Harrold: one artifact encoding the whole
//! population, with the choice deferred from compile time to process start. Testing a mutant then
//! costs one process launch instead of one build, and the guard itself costs a cached atomic load
//! and a branch the CPU learns immediately.
//!
//! Nothing is added to your manifest and nothing is instrumented in place: the workspace is copied
//! to a scratch tree, the guard runtime is vendored into it, and the rewriting happens there. See
//! [`cargo_gamma_rt`](https://docs.rs/cargo-gamma-rt) for the guard protocol and
//! [`cargo_gamma_attrs`](https://docs.rs/cargo-gamma-attrs) for the suppression attributes.
//!
//! See the repository README for the full command surface, and `docs/DESIGN.md` for the design.
//!
//! # Why this file is almost empty
//!
//! Code in a `[[bin]]` target cannot be linked by an integration test, so it is the least testable
//! code in a Rust project; and for a mutation testing tool it is also the code our own analysis is
//! weakest on, because no test links the target. Everything of substance lives in
//! `cargo-gamma-lib`, where it can be tested — including the argument parsing, the exit codes and
//! the console rendering, which a fake [`Host`] turns into ordinary assertions.
//!
//! What is left here is the one thing that genuinely cannot be tested: the real terminal and the
//! real process.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use cargo_gamma_lib::{Host, run};
use std::env;
use std::io::{IsTerminal, Write, stderr, stdout};
use std::process;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Host that talks to the real terminal and the real process.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealHost;

#[cfg_attr(coverage_nightly, coverage(off))]
impl Host for RealHost {
    fn output(&mut self) -> impl Write {
        stdout()
    }

    fn error(&mut self) -> impl Write {
        stderr()
    }

    fn is_terminal(&self) -> bool {
        stderr().is_terminal()
    }

    fn terminal_width(&self) -> Option<u16> {
        terminal_size::terminal_size().map(|(width, _)| width.0)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() {
    // `run` returns the process exit code rather than exiting itself, so that every code path
    // through the CLI is reachable from an ordinary integration test.
    process::exit(run(&mut RealHost, env::args_os()));
}
