# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The `expr.increment` and `expr.decrement` operators are now much more selective about what they
  offer to perturb. Adding one to an expression only makes sense when the expression is a number,
  and without type resolution the family had been accepting any shape that could plausibly be one —
  which in practice meant `(name) + 1` for every `String`, `(PhantomData) + 1`, and so on. Nearly
  two thirds of what it generated could not compile.

  Two things changed. Names the source writes a type for — function parameters and annotated
  `let`s — are now recorded and consulted, so a `String` parameter is not offered and a `usize` one
  still is. And a handful of shapes that say what they are without any inference at all are turned
  away: a bare `self`, an identifier spelled like a type rather than a value, a `Vec::`/`String::`
  associated call, and methods such as `iter`, `collect` and `to_string`.

  A name whose type the source never wrote down is still offered, deliberately: an unviable mutant
  costs a share of one rebuild, whereas a viable mutant dropped on a guess is a gap in the report
  that nothing else would ever reveal.

- The `fn_value` family no longer invents a `Default::default()` for a type nothing promises has
  one. The fallback is what it reaches for whenever it cannot name a value, and for a concrete type
  it has never heard of that optimism is usually repaid. For an abstract type it is not: a caller's
  `E`, an `impl Trait`, a `Box<dyn Trait>` and an associated type projected out of a type parameter
  such as `D::Error` are all chosen by somebody else, and a `Default` bound would be written in the
  signature if it held. On a serde-shaped API this was the single largest source of mutants that
  could not compile.

  Only the guess is withheld. A `Result<usize, D::Error>` keeps every `Ok` mutant it had and loses
  only the `Err` it could never have built. A parameter declared `T: Default` keeps its mutant,
  because there the promise was made explicitly. `Self::Value` is not treated as abstract even
  though it is spelled the same way, since inside an `impl` block it resolves to a type that block
  chose and frequently does have a `Default`.

  Measured together, the two changes above removed 245 mutants that could not compile and 12 that
  were suppressed anyway, out of a 1,196-mutant run — while leaving every mutant a test had
  actually killed exactly where it was. The share of generated mutants that fail to compile fell
  from 45% to 31%.

- The user-facing documentation is now a README plus four references, rather than one file trying
  to be a tutorial and a reference at once. `docs/OPERATORS.md` lists every one of the 105 mutators
  by family with its academic alias; `docs/PROFILES.md` covers all twelve profiles and says which to
  reach for; `docs/CONFIGURATION.md` documents every `.cargo/gamma.toml` key against the flag it
  corresponds to; and `docs/SUPPRESSION.md` gives the directive grammar, the seven places a
  directive may go, and a table comparing the five suppression channels.

  Previously the README's only catalog table listed ten of the twenty-three families and no
  individual mutator name at all, which made the names it calls "the vocabulary for the command
  line, the report, and every suppression channel" undiscoverable from the documentation. Eight of
  the twelve profiles were unmentioned, including `@parity` — the one a cargo-mutants user needs to
  make the two tools' scores comparable — and twenty-nine of the thirty-five configuration keys had
  no entry.

  The reference tables are generated from the registry and checked by a test, so a mutator or a
  configuration key added without a corresponding documentation entry now fails the build instead of
  quietly going unpublished. `GAMMA_BLESS_DOCS=1 cargo test --all-features --test docs` regenerates
  them.

### Fixed

- Mutants in code the build never compiles are no longer reported as survivors. A module behind an
  inactive `#[cfg]` — a feature that is off, or a platform that is not this one — still yields
  mutants, because mutants are found by reading source; none of them could change what a test
  observes, so every one of them survived. On a crate whose serialization support is an optional
  feature this was not a rounding error: the score read 58.9% instead of 91.1%, and 278 of the 330
  reported survivors were in code no compiler had read.

  After the build, gamma now consults cargo's dep-info to learn which files were actually compiled,
  and mutants outside that set are counted as `not built` rather than scored. Deferring to the
  compiler rather than re-evaluating `#[cfg]` ourselves means features, target platform and every
  other predicate are covered without reimplementing feature resolution. A run that excludes any
  mutant this way says so and names the remedy, since a population smaller than the one `gamma list`
  reports would otherwise be a mystery. The check works per file, so a `#[cfg]` on a single item
  inside a compiled file is still not detected.

### Changed

- Memory enforcement is now on by default, on the same footing as the wall-clock timeout. Each test
  binary's whole process tree is metered during the baseline, and every mutant of that binary is
  held to a ceiling derived from what it measured. A mutation can turn bounded allocation into
  unbounded allocation, and the user who most needs protecting from that is the one who never
  thought to ask. `--memory off` restores the previous behavior.

  Where the host cannot provide the accounting — no cgroup v2 delegation on Linux, or macOS at all —
  a run that merely inherited the default now continues unbounded and says so once on the diagnostic
  stream, rather than refusing to start. A run that named `--memory` or a size flag still stops with
  an error, because someone who asked for a guarantee is worse off believing they have one. The same
  split applies to `--no-baseline`, which leaves nothing to calibrate a ceiling from.

- A mutant stopped by its memory ceiling is now reported as `OUTOFMEM` and counted in its own column
  of the summary line, which reads `N caught, N missed, N timed out, N out of memory, N uncovered`.
  It was previously folded into `caught`. It still counts as detected, on the same reasoning as a
  timeout — the baseline established that the workload fits under the ceiling without the mutant —
  but the suite's assertions did not fail, and a reader who cannot tell those apart goes looking for
  a failing test that does not exist. It is also the outcome most likely to be wrong, since a
  ceiling set too tight convicts a healthy mutant, so the note carries the peak and the ceiling.

  In the JSON report the new outcome is exported as the schema's `Timeout` status, which is the
  closest the closed `mutation-testing-elements` enum offers: it is the other resource-exhaustion
  verdict and the only one that schema counts as detected. `RuntimeError` is the better-sounding
  name and the wrong answer, because the schema excludes it from the denominator, which would make
  the viewer's score disagree with the printed one.

- Every mutator in the catalog is now on by default. Previously `match_arm`, `struct_field`, `expr`
  and several others had to be named with `--ops` before they would run. A mutator that needs a flag
  is one nobody runs, and a gap in a mutation score nobody can see is worse than a mutant somebody
  has to spend a minute judging. Expect a larger population, a longer run and some mutants no test
  can kill; `--ops` still narrows a run when that is what you want. The `@control` and `@numeric`
  profiles are unchanged and remain the cheap way to audit one module.

### Added

- `--include-test <GLOB>` and `--exclude-test <GLOB>` choose which test *targets* decide a verdict,
  where `--test-package` could only work a package at a time. Patterns match cargo target names with
  `*` and `?`, so a package's unit tests are named after the lib or bin they live in and each file
  under `tests/` is a target named after the file. Exclusion is applied after inclusion and always
  wins. The same keys are available in `.cargo/gamma.toml` as `include-tests` and `exclude-tests`.

  This exists for the corpus that shares a package with a real test suite but is not an oracle —
  conformance targets, fuzz seeds, golden-file comparisons. Their failures say nothing about whether
  a mutant was noticed, and until now the only way to keep them out was `--test-package`, which
  would have dropped the package's genuine tests along with them.

  A pattern naming no declared test target is a usage error rather than a silent no-op, on the same
  reasoning as an unmatched `--file`: an `--exclude-test` typo leaves the target in the oracle, so
  mutants it would have let through are reported as caught and the score reads better than the suite
  deserves, which in CI is indistinguishable from a run that went well. Patterns are checked against
  what the workspace declares rather than what this run built, so one naming a target gated behind
  `required-features` keeps working when those features are off. Filtering happens before the
  baseline, so the per-binary timeout shares describe the suite that actually runs, and a run that
  dropped any target says so on a line beginning `Oracle`.

- Return-value replacement now recurses through the return type instead of stopping at the outer
  constructor. A `Result<Option<bool>, E>` yields `Err(Default::default())`, `Ok(None)`,
  `Ok(Some(true))` and `Ok(Some(false))` where it previously yielded a single `Default::default()`,
  and the same recursion covers tuples, `Vec`, `VecDeque`, `HashSet`, `BTreeSet`, `BinaryHeap`,
  `LinkedList`, `HashMap`, `BTreeMap`, `Box`, `Rc`, `Arc`, `Cow` and the `NonZero` types. Depth and
  width are bounded so a deeply generic signature cannot generate an unbounded population. This
  closes the last capability `cargo-mutants` had that `cargo-gamma` did not.

- Six mutator families for Rust's own value and standard-library semantics. `option` and `result`
  swap `Some` for `None`, `Ok` for `Err` and back, asking whether the present case is distinguished
  from the absent one; `iter` swaps `any`/`all`, `min`/`max` and `first`/`last`, and deletes an
  in-place `sort` or `dedup`, asking whether anything observes that a sequence was ordered;
  `string` swaps `starts_with`/`ends_with`, `to_lowercase`/`to_uppercase` and
  `trim_start`/`trim_end`; `collection.omit_element` drops one element from a `vec!` literal; and
  `assign_value.default` replaces the right-hand side of an assignment with `Default::default()`.

  Only swaps whose two spellings share a *type* are offered, because a mutant is one arm of an `if`
  whose other arm is the original. `take`/`skip` and `take_while`/`skip_while` ask real questions
  but produce different adapter types, so they are absent rather than generated and withdrawn on
  every run. For the same reason a function returning `impl Iterator` gets no return-value mutants.

- Peak memory measurement and opt-in memory ceilings for test binaries. `--memory measure` records
  what each test binary's whole process tree uses during the baseline and reports it in the run
  summary and in `--diag`. `--memory enforce` additionally holds every mutant to a ceiling derived
  from that binary's own baseline peak — the larger of `--memory-multiplier` times it (default 2)
  and it plus `--memory-headroom` (default 128MiB) — and reports a mutant the kernel stops for
  reaching it as caught, with a note naming the binary, the peak and the ceiling. `--memory-limit`
  states a ceiling outright, and is the only way to bound a run that also passes `--no-baseline`.
  `--baseline-memory-limit` bounds the baseline runs themselves. The same settings are available in
  `.cargo/gamma.toml` as `memory`, `memory-multiplier`, `memory-headroom`, `memory-limit` and
  `baseline-memory-limit`.

  Enforcement is implemented with a freshly created cgroup v2 leaf per invocation on Linux and with
  job objects on Windows, so that it accounts for the whole descendant tree rather than the direct
  child. Both are off by default. Where the host cannot provide them — a Linux session without
  cgroup delegation, macOS, anything else — a run that asked for them stops with the reason instead
  of continuing unprotected.

- Five new mutator families reach places the expression-level rewriters cannot. `match_guard`
  negates a guard or forces it either way; `match_arm.never_matches` stops an arm from matching, so
  control falls through to the wildcard; `struct_field.omit` drops a field from a struct literal and
  lets the base expression supply it; `range` moves a range's endpoint by one, which is what
  swapping `..` for `..=` means; and `loop` swaps
  `break` for `continue`, and deletes either. Match arms, match guards and struct fields were the
  three places `cargo-mutants` covered and this tool did not, and each of them is where a
  dispatch table quietly stops dispatching.

  `match_guard.negate`, both `range` mutators and `loop.continue_to_break` are on by default. The
  rest are opt-in, because they fire on nearly every match and every struct literal in a codebase
  and the cost is a full build round each.

- An opt-in `expr` family perturbs a numeric expression by one where being off by one is
  survivable-looking but wrong: argument position, index position, and the returned value of a
  function whose return type is numeric. It is deliberately not applied to literals, which the
  `literal` family already covers, nor to the result of a `with_capacity`-style call, where the
  number is a hint and changing it cannot fail a test.

- Two profiles: `@control` (`cond`, `match_guard`, `match_arm`, `loop`) selects everything that
  changes which code runs rather than what it computes, and `@numeric` (`literal`, `expr`) selects
  everything that changes a number.

- A hidden `--diag` dumps what a run measured about itself: the wall-clock split, effective
  parallelism against `--jobs`, the outcome histogram, the slowest mutants, per-mutator,
  per-package and per-file cost rollups, and the test-binary table. It exists so that a change to
  the scheduler or the mutator catalog can be judged against numbers instead of against how the run
  felt. Nothing about it is stable and none of it is meant to be parsed.

- `--advice <PATH>` writes the diagnosis and the per-family yield table as Markdown.

- The GitHub Actions job summary now carries the diagnosis and the yield table alongside the score.
  The panel a team reads every morning said what the score was and never what to do about it, which
  is how a nightly run becomes something people scroll past.

- `-V`/`--unviable` lists the mutants that could not be compiled. The summary has always counted
  them; a large workspace produces thousands, and printing every one buried the survivors that are
  the actual result, so the list is now opt-in.

### Fixed

- `cargo gamma migrate` no longer discards settings that have an exact gamma equivalent. It
  translated 8 cargo-mutants keys and reported the rest as having "no equivalent"; it now
  translates 13, including `profile`, `features`, `all_features`, `no_default_features`,
  `additional_cargo_args`, `error_values` and `test_package`. On a real cargo-mutants workspace
  this was the difference between 2 keys carried over and 4. Losing a setting during a migration is
  worse than failing to migrate, because the run afterwards looks like it worked.

  `minimum_test_timeout` had been mapped onto `timeout`, turning a floor on the derived per-mutant
  budget into a fixed budget for every mutant. It now maps to `minimum-test-timeout`, which is the
  same setting under a different name.

  `additional_cargo_test_args` is deliberately not translated onto `cargo-test-args`, which looks
  like the match and is not: the cargo-mutants key passes arguments to `cargo test`, gamma's passes
  them to each test binary. The generated line now explains that, because the alternative was a
  config that placed `--tests` in front of libtest and failed at the baseline.

  Keys that genuinely need nothing are now reported separately from keys gamma does not recognise,
  each with the reason it needs nothing — `cap_lints` and `gitignore` because gamma always behaves
  that way, `output` and `sharding` because gamma spells them differently. Previously both kinds
  were reported as doing nothing, which is true of only the second.

- A `--file` or `--exclude-file` pattern naming a package the run does not mutate is no longer an
  error. Patterns are checked against every source file in the workspace rather than only the files
  the run selected, so a `gamma.toml` written once for the whole workspace keeps working under
  `--package`. A pattern that matches nothing anywhere is still an error, which is the typo the
  check exists to catch.

### Removed

- The `advise` subcommand and `--yields`, both replaced by `--advice <PATH>`. `advise` was a run
  that also diagnosed, and `--yields` was half of that diagnosis printed on its own, so one
  analysis was spelled three ways. It is now written as a Markdown document, which is what it
  always was: prose with remedies, meant to be read later, shared, and pasted into a review rather
  than scrolled past in a terminal.

- The `estimate` subcommand, in favour of `--estimate` on `run`. It was a run that stopped early,
  which meant paying for the build and the baseline — nearly all of the fixed cost — and then
  throwing the tree away without testing anything. The flag prints the same projection at the same
  moment and then carries on with the run, so the measurement is no longer spent twice.

- `--no-estimate`. It suppressed a projection block that no longer exists: the time a run has left
  is now part of the progress gauge, which `--progress` already governs.

### Changed

- Line coverage of the library rose from 93.4% to 99.7%. Three systematic gaps accounted for
  almost all of the shortfall.

  Every command module carried its own `Host` test double with methods no test in that module
  called; they are now one shared `crate::testing` module, exercised by its own tests.

  Every `?` on a `writeln!` was unreachable because tests write into a `Vec<u8>`, which never
  fails. A `FlakyHost` now walks the failure point down a function one line at a time, so each `?`
  is proved to propagate rather than assumed to.

  The process-level paths — a suite that hangs, one that goes silent, a binary that will not start,
  a build that fails for a reason no guard explains, a narrowed build that has to widen — had no
  test at all. They are now driven with real processes and real cargo invocations against
  throwaway one-crate workspaces, which run in under a second each. Where the obstacle was an
  ambient read rather than a real process, the read moved into a parameter: `loader_path` and
  `measure_baseline` take the inherited search path and the budget rather than reaching for the
  environment and a hard-coded constant.

- The crate-level documentation of all three published crates now covers the whole surface with
  worked examples: `cargo-gamma` documents every subcommand, the mutator families and profiles, the
  selector language, scoping, sharding, all four suppression channels, the reports, the CI wiring
  and the exit codes; `cargo-gamma-attrs` documents each attribute and each selector shape; and
  `cargo-gamma-rt` documents the guard protocol end to end. The examples in the two library crates
  are doctests, so the documentation cannot drift from what compiles.

- The summary line no longer counts unviable mutants. They are a fact about what the compiler would
  accept rather than about what the tests check, they are withdrawn automatically, and a large
  workspace produces thousands of them — a number nobody acts on, sitting on the one line everybody
  reads. `--unviable` lists them and `--diag` counts them.

- A timed-out mutant is no longer told it timed out twice. The line was suffixed with `: ran out
  its budget`, which the `TIMEOUT` label already said. A mutant that stalled still names the test
  it hung in, because that is the one thing the line cannot be read off the mutant itself.

- A narrow terminal now drops the running verdict counts from the progress line instead of
  truncating it. Truncation takes columns from the right, which is where the time remaining lives,
  so a terminal a few columns short lost the most useful part of the line and gained an ellipsis.
  The counts are recoverable from the survivors printed above and from the summary.

- The progress line marks the time remaining with `~` rather than spelling out `estimating`, which
  bought the reader nothing and cost eleven columns on the line most likely to run out of them.

- `--advice` now writes a navigable document rather than a flat list: a title, a table of contents,
  a table of what the run cost against what it decided, numbered findings, and a glossary of the
  verdicts. The file gets forwarded to people who did not run the tool, and a wall of headings with
  no structure is one nobody reads past.

- The run ends on one line. `Found`, `Skipped`, `Summary`, `Also`, `Timing`, `Hangs` and `Rollback`
  were seven lines of bookkeeping around one number, and a reader looking for the result had to
  find it among them. It is now
  `Summary 303 mutants (294 caught, 7 missed, 2 timed out, 0 uncovered => 97.7%)`. All four
  verdicts are always named, zero or not, so the line keeps a shape that can be scanned instead of
  read, and they sum to the population in front of them; counts for suppressed, sharded-out and
  already-settled mutants are appended only when they are not zero. `--estimate` and `--advice` are where a run's
  mechanics are reported now.

- A missed mutant means a test ran it and did not notice, and nothing else. An uncovered mutant
  still costs score, but it was folded into the missed count before, which sent readers looking for
  an assertion that was never going to be there. It is counted on its own.

- `cargo gamma fix` is now `cargo gamma suppress`, and `--dry-run-fix` is `--dry-run-suppress`.
  It never fixed anything: it writes `gamma::skip` directives for mutants that cannot usefully be
  tested. Generated directives are tagged `written by cargo gamma suppress`.

### Fixed

- Code the compiler never sees is no longer mutated. A mutant behind a `cfg` predicate that does not
  hold has no guard in the binary, so activating it changed nothing, every test passed, and it was
  reported as a survivor — after spending a full test run to learn that. On a workspace with a lot
  of platform- or feature-conditional code this was both the largest source of false survivors and a
  large fraction of the run's cost. Predicates are now evaluated against the real cfg set from
  `rustc --print cfg` together with the features the run enables, using three-valued logic so that
  anything unmodelled leaves the code mutable.

- `merge` now drops a verdict for code that no longer exists. Merging unioned by identity and kept
  the newest verdict, which meant nothing was ever removed: a survivor whose code had since been
  edited stayed in the denominator forever and rendered over a line its construct had left. When an
  input is unsharded it states the whole population of the files it covers, and an identity missing
  from the newest such input is withdrawn and reported under `Withdrawn`. A sharded report describes
  only its own slice, so it never withdraws anything. Relatedly, merging a listing with a run no
  longer blanks the run's verdicts.

- `list mutants --json-report <PATH>` writes the population as a report document, so a nightly
  rotation can state what currently exists without paying for a full run.

- The runtime guard no longer allocates. It read `GAMMA_ACTIVE` through `std::env::var`, which
  returns a `String`; the read now goes through the platform environment API into a fixed buffer and
  caches the answer in an atomic. The guard runs on every mutated expression in every test, and an
  allocation there is charged to the whole suite — precisely the cost the schema exists to avoid.

- Doctests are documented as being outside the model, in both the README and the design document.
  They are not built, run, timed or reported, so coverage carried only by a doc example shows up as
  `uncovered` and the score is a lower bound on such a crate. Running them would mean a compile and
  link per doctest, which would undo the one-build economy the tool is built around.

- A stalled mutant no longer leaves its descendants running. The run killed only the process it
  started, so anything a test had spawned survived — holding locks inside the scratch tree, which
  failed the *next* run, and holding inherited pipe handles, which kept whoever was reading this
  tool's output from ever seeing end of file. The child now leads its own process group on Unix and
  sits in a job object on Windows, and both are killed whole. Interrupting a run takes the children
  with it: a group of its own is a group the terminal's `Ctrl-C` no longer reaches, so the run asks
  for them explicitly instead of relying on the accident that used to do it.

- Scratch build output is deleted when a run fails. Artifacts were kept unconditionally so that the
  next run could be incremental, but the artifacts of a run that never produced a result cannot make
  anything incremental — and on a large workspace they reached tens of gigabytes, which is more than
  a hosted CI runner has free. A successful run still keeps them, and a run that leaves more than ten
  gigabytes behind now says so.

- `--cap-lints=allow` is merged into the scratch tree's `.cargo/config.toml` instead of being pushed
  through `RUSTFLAGS`. Setting the variable *replaced* whatever flags the tree had configured, so a
  workspace that set `--cfg` flags or a target CPU silently compiled into something other than what
  its tests were written against. An ambient `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` outranks
  configuration, so when the caller has one it is extended rather than replaced.

- `--rollback-rounds` replaces a hard-coded cap of 32 rounds, and the failure now reports what the
  last few rounds withdrew — including telling you when more rounds would not have helped.

- The CI surfaces are capped at what GitHub accepts rather than at guesses: ten annotations per step
  instead of fifty, and five thousand SARIF results within ten megabytes instead of twenty-five
  thousand with no size limit at all. A log over the byte limit is shrunk until it fits, because an
  oversized upload is rejected whole rather than trimmed.

- `cargo gamma suppress` preserves a file's line endings. Generated directives were written with a
  lone LF regardless, so editing a CRLF file produced a mixed-ending diff on lines nobody touched.

- The projected worst case now includes the confirmation run a suspected timeout or stall is put
  through, so it is a ceiling a real run cannot walk past rather than a number it can exceed several
  times over. The projected range is capped at it.

- A stall no longer claims to name the test that hung. libtest runs tests in parallel and announces
  each one only when it finishes, so the name reported is whichever test finished last before the
  silence — by definition not the one still spinning. The wording, the README and the design notes
  now say so.

- The exit-code contract is documented, including that surviving mutants do not fail the process
  unless `--min-score` is set.

- `docs/DESIGN.md` matches the implementation again: the real statement-mutator names, the `ignored`
  outcome, `replacement_index` and the 48-bit truncation in a mutant's identity, the confirmation
  budget, and a corrected claim about how the schema's size grows.


- The HTML report no longer renders a white page behind a dark report. The viewer themes its own
  components but not the page around them, so the background stayed white while everything on it
  went dark. The page now declares `color-scheme` and follows the theme the viewer settles on,
  including one picked inside the report that disagrees with the system.

- Only the test targets that can actually return a verdict are compiled. `-p` narrowed what got
  mutated but not what got built, so a run scoped to one crate still compiled, baselined and then
  ignored the test binaries of every unrelated crate in the workspace. The build now asks cargo for
  the packages whose tests can reach something being mutated, honouring `--test-package` and
  `--test-workspace`. Because cargo unifies features over the packages it is told to build, a
  narrowed build that fails for a reason no mutant can be blamed for is retried across the whole
  workspace rather than reported, and the run says so on a `Scope` line so the wasted build is not
  a mystery.

- A path dependency on a sibling package is no longer redirected out of the copied tree. Any
  relative path leaving its own package directory was anchored back to the original workspace, but
  the copy had brought the sibling along, so cargo saw one package at two locations and refused to
  write a lockfile. Only paths that leave the copied tree are anchored now. Every workspace
  declaring `path = "../sibling"` in a member manifest failed outright before this.

- An error raised mid-phase starts on its own line. A phase writes what it is about to do and
  holds the line open until it can say what it found, so a phase that failed instead left the line
  open and the error was printed as the rest of that sentence: `Baseline building the test
  binaries and running the suiteerror: ...`.

- Timeouts are reported as they happen rather than in a block at the end. A timeout is the most
  expensive thing a run can find, and it was the one outcome held back until the summary.

- Survivors and timeouts are no longer printed twice on a terminal, once live and again in the
  summary. The duplicate listing pushed the `Found` and `Summary` lines into the middle of the
  output. Piped output is unchanged: with no live display, the summary is the only place the
  results appear, and results belong on stdout.

- The projection no longer assumes every mutant runs the whole suite. It multiplied the
  whole-workspace baseline by a constant for every mutant, but a run only tests a mutant against
  the binaries that can link its package, so on a loosely coupled workspace the estimate was too
  high by roughly the number of crates in it. It now sums, per mutant, only the binaries that can
  actually reach it. The worst case is computed the same way.

- The projection is one line rather than a five-line block. It is printed mid-run, with the build
  and baseline timings it used to repeat already on the screen directly above it.

- `cargo gamma advise` no longer gates on `min-score` or overwrites the report files configured in
  `.cargo/gamma.toml`. A diagnosis is not a verdict, and failing the process because the score is
  low is exactly the situation that sends someone to `advise` in the first place.

### Changed

- The progress gauge now reports what the run has found rather than the mutant it happens to be on:
  `Testing [====>    ] 412/18751 mutants evaluated (7 missed, 2 timeouts), estimating 24m to go`.
  The gauge is a fixed width, because sizing it from whatever the caption left over collapsed it to
  four columns as soon as the caption grew. It also counts mutants rather than estimated cost — a
  gauge nobody can convert back into a number of mutants is not telling them anything — and the time
  left is extrapolated from the rate actually achieved so far, which absorbs the job count, the
  machine, and the share of mutants that hang without having to model any of them.

- The build now processes one package at a time, in dependency order, instead of scanning the whole
  workspace and then converging over all of it at once. Each package is scanned, instrumented, built
  with `cargo build -p`, and rolled back until it compiles before the next one is attempted, so a
  crate is rebuilt for its own unviable mutants rather than for every other crate's. The run reports
  each package once, as it finishes, carrying what that package yielded and what it withdrew,
  instead of naming every package twice and going silent in between. A final workspace build still
  has the last word — it is the one that compiles under real feature unification and produces the
  test binaries — so verdicts are unchanged.

- Each package is announced before it is scanned and reports what it yielded on the same line once
  it is built, so the run reads as one line per package rather than two. Building the test binaries
  and measuring the baseline are likewise one line: neither half means anything without the other.

- The count a package reports is the mutants that survived compilation rather than the mutants that
  were found. How many did not compile is a fact about the tool rather than about the code, and the
  summary already accounts for all of them once.

### Fixed

- A file reached only through a `#[cfg(test)]` module declaration was mutated as though it were
  real code. The collector drops `#[cfg(test)]` items wherever it sees them, but

  ```rust
  #[cfg(test)]
  #[path = "reader_tests.rs"]
  mod tests;
  ```

  puts the attribute in one file and the code in another, and files were parsed independently, so
  nothing in `reader_tests.rs` said it was test code. The module tree is now walked from each crate
  root, and a file reached only through a test-gated declaration — along with everything below it —
  is left alone. On one workspace this removed twelve mutants that were being run against assertions,
  where nothing could meaningfully catch them, and fourteen that were being reported as unviable.

- A rollback round no longer rewrites files whose mutants did not change. Cargo decides what to
  rebuild from mtime rather than content, so rewriting a file with the text it already held forced
  its crate and everything downstream of it to recompile — which made every round cost a full
  workspace build regardless of how few mutants that round withdrew.

- A package with a relative path dependency pointing outside its own directory — `shared = { path =
  "../shared" }` — failed the whole run. The scratch copy does not sit where the original did, so
  the path resolved to somewhere that does not exist, and the run reported an unattributable
  compile failure rather than the missing crate. Every manifest in the copied tree now has its
  escaping paths anchored back to the original, covering `[dependencies]`, `[dev-dependencies]`,
  `[build-dependencies]`, `[replace]`, `[patch.*]`, `[target.*]` and `[workspace.dependencies]`, as
  well as the `paths` overrides in `.cargo/config.toml`. Paths that stay inside the tree are left
  alone, so the copy remains self-contained wherever it can be.

- A symlinked directory in the workspace was followed and copied as real files, so a link pointing
  outside the workspace copied whatever it pointed at. Links are now recreated as links.

- A tree deeper than 64 directories was silently truncated: the copy stopped and reported success,
  and the missing files surfaced later as a build failure naming something unrelated. There is no
  longer a depth limit — symlinks are not followed, so there is no cycle to guard against.

- An unreadable file, or one whose name is not valid UTF-8, was skipped without a word. Both are
  now reported.

- The build timeout was checked after cargo returned, so it could report an over-long build but
  never stop a hung one. The build is now cut off when its budget runs out.

### Changed

- The workspace copy skips files ignored by version control, and recognises all seven common
  version control directories rather than only `.git`. Ignore rules are read only from inside the
  tree and only when it is a real repository, so a checkout nested under a directory whose
  `.gitignore` says `*` still copies.

- Files are cloned rather than copied where the filesystem supports it, and the copy runs in
  parallel.

- Instrumented sources are no longer written through a symlink, or created where the copy did not
  put a file. Either would mean writing somewhere the copy did not choose.

- The rollback loop converges with `cargo build --tests --keep-going`, so one round collects
  diagnostics from every crate that can be compiled rather than stopping at the first that fails.
  This matters most on a wide dependency graph; on a sixteen-crate workspace it was worth one round
  out of nine, so the number of rounds is dominated by something else.

- Discovery reports each package once its files have been parsed, carrying what that package
  actually yielded — `Scanning exemel-xsd, 8881 mutants in 14 files` — rather than naming packages
  while merely enumerating their files. The size of the parse is announced before it starts.

- The baseline reports how many tests ran and how long they took, which are the figures the mutant
  timeout and the stall budget are derived from.

### Added

- `--scratch-dir` puts the copied tree and its build artifacts somewhere other than the workspace's
  own `target` directory, so a read-only checkout can be mutated, a slow filesystem can be avoided,
  and concurrent runs can be given somewhere separate to work.

- A run holds a lock on its scratch directory. Two runs sharing one used to delete each other's
  tree and write artifacts into a single directory under two different sets of instrumented
  sources; the second is now turned away. The lock is released by the operating system, so a crash
  leaves nothing to clear.

- `-v`/`--caught` lists the mutants the suite killed, not just the ones that escaped.

- Cargo feature selection: `--features`, `--all-features` and `--no-default-features`. These reach
  discovery as well as the build, so the mutants found and the tree compiled agree about which code
  exists.

- `-p`/`--package` and `--workspace` choose which packages get mutated. Naming a package that is
  not in the workspace is an error rather than a run that quietly finds nothing.

- `--test-package` and `--test-workspace` choose which packages' tests decide a verdict, which is
  separate from which packages get mutated.

- `-D`/`--in-diff` restricts the population to the lines a unified diff touches, reading `-` as
  standard input. This is what makes a run affordable on a pull request; a shard is a slice of
  everything and is not a substitute.

- `--profile` selects the cargo profile. A run builds once and then executes thousands of mutants,
  so paying for an optimized build usually wins.

- `-C`/`--cargo-arg`, `--cargo-test-arg` and a trailing `-- <args>` pass through to cargo and to the
  test harness.

- `--minimum-test-timeout` puts a floor under the computed timeout, so a fast suite on a loaded
  machine does not report scheduling noise as a hang. `--build-timeout` and
  `--build-timeout-multiplier` bound the build, which a run pays for exactly once.

- `--iterate` skips the mutants a previous report already settled, turning a long run incremental.
  Killed, timed-out, unviable and ignored mutants are settled; survivors are always retried, because
  the next run's tests may kill them.

- `--error` generates `Err(v)` return mutants from caller-supplied values, reaching error types that
  do not implement `Default` and so were beyond `fn_value.err_default`. The new mutator is
  `fn_value.err_with`, which supplying any value turns on.

- `--leak-dirs` keeps the scratch tree after a run and prints where it is.

- `--config <FILE>` points at a configuration file elsewhere, and `--no-config` runs with none. An
  explicitly named file that does not exist is an error, unlike an absent conventional one.

- A `completions <SHELL>` subcommand.

- `migrate --command` now translates all of the above from their `cargo-mutants` spellings.

### Changed

- The scratch tree is deleted at the end of a run. It previously stayed at `target/gamma/tree`;
  use `--leak-dirs` to keep it.

- Discovery now parses source files across all available cores instead of one at a time. Parsing is
  what discovery spends its time on and nothing in one file informs another, so on a sixteen-core
  machine a 108,000-line workspace went from 5.6s to 0.7s. Files are claimed one at a time rather
  than in fixed blocks, since a static split leaves the machine waiting on whichever worker drew the
  largest ones, and results are put back in file order so the population does not depend on how the
  work happened to land.

### Fixed

- The status column collapsed whenever color was on. A format width counts bytes, and a styled
  label is mostly escape sequences, so `{label:>12}` measured the escapes and padded to nothing —
  meaning the alignment was correct only when piped to a file and wrong on every real terminal.
  Alignment now happens before the styling does.

- `Instrumenting` was one character wider than the status column and pushed its line out of
  alignment even without color. The phase is now `Rewriting`.

- Counts are pluralized: `1 file` rather than `1 files`.

- `Preparing copying the workspace` read as two collided gerunds. It is now `Copying the
  workspace`.

- Continuation lines under the estimate were indented one column short of a clean sub-item.

- A survivor was announced as `Survived` while it was found and listed as `MISSED` in the summary.
  Both now say `MISSED`.

- Every test process now runs with at least 16 MiB of stack. Instrumentation enlarges stack frames,
  so a deeply recursive test that fitted in the default 2 MiB could overflow only under mutation,
  failing the baseline and losing the run.

- A borrowed array of literals — `fn f() -> &'static [&'static str] { &["id", "name"] }` — compiles
  only because a constant can be promoted to static storage. Instrumenting one of those literals
  made the array non-constant, so it became an ordinary temporary and the borrow no longer outlived
  the function. The tree failed to build, and because the resulting borrow-check error is reported
  against the whole enclosing expression rather than the mutated site, no mutant could be blamed for
  it and the entire run was lost. Such borrows are now left uninstrumented.

- Compile errors that point at a region *containing* mutation sites, rather than at one, can now be
  attributed. Every mutant inside the smallest such region is withdrawn and reported as unviable.
  Previously only a guard enclosing the diagnostic counted, so a whole class of borrow-check and
  type errors was unattributable and aborted the run instead of costing a few mutants.

- Conditions that bind a pattern as part of a let-chain — `if let Some(n) = x && y` — were mutated
  as though they were ordinary booleans. Negating one, replacing it with `true` or `false`, or
  turning its `&&` into `||` either fails to parse or leaves the bindings the body depends on
  unbound. Only the top-level `if let` form was recognized before; the binding may sit anywhere in
  the `&&` spine.

- Test threads now get a 16MiB stack unless `RUST_MIN_STACK` already asks for more. Every mutation
  site becomes a branch holding both the original expression and its replacement, so instrumented
  frames are larger than the ones they stand in for, and deeply recursive code that fits in the
  default 2MiB could exhaust it. The process aborts on a stack overflow, which read as the whole
  suite failing rather than as anything to do with a mutant.

- Pairing mutants with the files they belong to scanned the whole population once per file, in the
  instrumenter, the report builder and the compile-error attributor. All three are now grouped once.
  The instrumenter runs on every rollback round, so a large workspace paid for it repeatedly.

- Discovery compared each newly walked file against every file already found, which is quadratic in
  a workspace with many source files.

- Building the population copied it twice — once to separate suppressed mutants and once to put them
  back — which doubled peak memory for no gain. Ordinals are now assigned in place. Peak memory for a
  population of 840,000 mutants fell from 818MB to 677MB.

- The counter that gives identical sites distinct occurrence indices kept an owned copy of the item
  path and the normalized source text of every site it had seen, neither of which was ever read
  back. It is keyed by a 128-bit digest now.

- The syntax-tree walk built replacement text before checking whether the mutator that would use it
  was even switched on, and copied both operands of every binary expression in the tree whether or
  not any of them were. The operator tables allocated a vector per expression visited. None of this
  allocates now unless a mutant is actually produced.

 The scratch-tree copy classified
  entries without following links, so a linked directory looked like a file and copying it failed
  outright. Links are now followed, with a depth limit so a cycle cannot recurse forever.

- A source directory named `target` was skipped when the scratch tree was copied, at any depth. A
  mutated file inside one then had nowhere to be written and the run aborted. Only the workspace
  root's `target` is skipped by name now; a nested one must carry the `CACHEDIR.TAG` cargo writes.

- A package that depended on the guard runtime only as a dev- or build-dependency was treated as
  already linked, and no normal dependency was added. The lib target cannot see either, so every
  guard in library code would have failed to resolve and the whole build with it.

- `merge` took each file's source text from the last report in argument order rather than the most
  recent one, contradicting its own documentation and the timestamp-based rule its verdicts already
  followed. A shell glob orders by filename, so merged reports could render fresh verdicts over
  source from an older commit.

- A suspected timeout or stall is now confirmed by a second run under a budget three times larger
  before it is believed. Both verdicts count as a detection, so a false one did not merely lose
  information — it inflated the mutation score by crediting the suite with a kill it never made.
  They are also the two verdicts a loaded machine can produce unaided: the budget is calibrated
  from a baseline measured while nothing else competed for cores, but mutants run many at a time.
  A suspected stall is retried with a looser silence budget rather than none, so a mutant that
  really has hung is still cut off early rather than waiting out the whole timeout.

- A mutant's budget is now divided among the test binaries it runs, in proportion to their share of
  the baseline. The budget is calibrated from the baseline, which times the whole suite, but was
  applied to each binary in turn — quietly granting a mutant as many times its budget as there were
  binaries, and making every projection built from it wrong by the same factor. The floor is
  divided the same way, so the parts still add up to the whole.

- A test binary that outlives its own children can no longer hang the run. The reader draining the
  binary's output was joined once the child exited, on the assumption that the child exiting closes
  the pipe. It does not: anything the test spawned inherited the write end, and a surviving
  grandchild holds it open indefinitely, so the join would never return and the entire run would
  stop. The wait is now bounded and the reader abandoned if it does not finish.

- A mutant that receives no guard is now reported as an internal error rather than silently scored.
  Such a mutant would still be run, with nothing in the tree to make it behave differently, and its
  verdict recorded as a survivor — a wrong answer that nothing downstream could distinguish from a
  real one.

- Two mutants at the same span but of different shapes are no longer merged, which would have
  wrapped the second in the first's guard — an expression guard around a statement, for instance.

- `'\''` no longer confuses the source scanner. The escaped quote was mistaken for the closing one,
  leaving the scanner a quote out of step and able to miss a suppression comment later on the line.

- A package id ending in a pre-release or build-tagged version, such as `#1.0.0-beta.1`, is no
  longer read as a package name. Doing so attributed a test binary to a package that does not
  exist, which stopped it from being considered able to reach any mutant.

- The progress display now moves during the mutant phase. Verdicts were collected into a shared
  vector and only reported once every worker had joined, so the bar sat frozen for the whole run —
  minutes, on a workspace of any size — then emitted every event at once. On gamma's own workspace
  the entire display arrived in the last 0.1s of a 216s run. Workers now publish each verdict over
  a channel that the calling thread drains while they are still running. Survivors are therefore
  printed as they are found, as was always intended, and the bar advances continuously. Console
  ordering now follows completion rather than plan position; the report files are unaffected, as
  verdicts are still recorded against the plan in place.

- Integer literal mutants no longer overflow. The `literal.int_increment` and
  `literal.int_decrement` operators added and subtracted one without checking, so a source literal
  of `9223372036854775807` panicked in a debug build and, worse, wrapped silently in a release one
  — offering a "+1" mutant numerically smaller than the literal it replaced. Literals at the
  extremes of the range now simply yield no mutant in that direction.

- Out-of-range numeric options are now rejected with a message instead of panicking. `--timeout`,
  `--timeout-multiplier` and `--min-score` accepted any float, including negatives, NaN and values
  large enough to overflow a `Duration`, which surfaced as a raw Rust panic from deep inside the
  standard library. The same values supplied through `.cargo/gamma.toml` bypassed argument parsing
  entirely and are now validated when the file is read.

- Compile errors in the instrumented tree are now attributed to the mutant that actually caused
  them. Attribution previously matched a diagnostic against a mutant's *source* line, on the
  assumption that a guard occupies the same line as the code it wraps. It does not: a guard emits
  the mutated text alongside the original, so a multi-line site grows and every later line shifts.
  Guards are now located in the instrumented text itself, and a diagnostic is blamed on the guard
  whose mutated branch contains it — an exact answer, since that branch is the only text in the
  tree that is not a copy of the original and no two such branches overlap. On gamma's own
  workspace this cut mutants withdrawn as unviable from 604 to 216 and rollback rounds from 14 to
  3. The mutants no longer withdrawn were always viable and are now tested and scored, so mutation
  scores will move. In its worst form the old behaviour could fail to attribute a diagnostic at
  all, which abandoned the entire run.

- Literals in pattern position no longer produce mutants. `syn` models a literal pattern as an
  ordinary literal expression, so match arms such as `"skip" => …` offered themselves as mutation
  sites; a guard is an `if` expression and no expression is legal in a pattern, so every one of
  those mutants was uncompilable. Patterns are now skipped entirely, as nothing in one is
  evaluated.

- `GAMMA_ACTIVE` is now scrubbed from the environment of every cargo invocation, so no mutant can
  be live during the build. Guards are inert only while that variable is unset, and proc macros run
  inside the compiler: a live mutant in one would be executed by rustc rather than by the suite, so
  a mutated macro that looped forever would hang the build with no diagnostic to roll back and no
  test to time out. Since gamma builds once, that would have cost the whole run rather than a single
  mutant. The variable is scrubbed rather than assumed absent, because gamma's own test processes
  set it. Asserted by a test.

### Changed

- The default `--timeout-multiplier` is now 1.2 rather than 5.0. A hung mutant costs its whole
  budget, and that cost is paid once per hang across the population, so a generous multiplier was
  one of the few remaining ways a run could take far longer than its cost model predicted. The
  20-second floor still protects fast suites from reading scheduler noise as a hang, and stall
  detection catches the common hangs well before the budget expires. Suites with genuinely variable
  timing can raise it again with `--timeout-multiplier` or the `timeout-multiplier` config key.

### Added

- CI surfacing: `--sarif` writes a SARIF 2.1.0 log of surviving mutants for GitHub code scanning,
  with the stable mutator names as rule IDs so alerts group and dismiss per operator, and the
  content-addressed mutant ID as a partial fingerprint so an alert survives reformatting.
  `--annotations` defaults to `auto`, which detects a GitHub Actions runner and then writes
  survivors to the diff as workflow annotations and a score table to the job summary. All three
  surfaces publish survivors only.

- Package-reachability filtering: a test binary is only run against mutants in packages its own
  package links, since Rust cannot execute code it does not link. The remaining binaries are tried
  cheapest-first, using per-binary baseline timings, so a killable mutant is usually killed by the
  fastest binary that could kill it. Mutants no binary can reach are reported `uncovered` rather
  than `survived`.

- Mutant model with content-addressed identity: a mutant's ID is derived from its file, item path,
  mutator, normalized site text and occurrence index, not its line and column, so it survives
  reformatting and edits elsewhere in the file.
- Mutator registry of 66 operators across 11 families, each with a stable `family.transform` name,
  plus 9 profiles and the academic aliases used in the mutation testing literature.
- Selector language for `--ops` and for suppressions: names, family prefixes, `@profile`, aliases
  and `all`, combined left to right with `!` for removal. Unmatched selectors are hard errors and
  carry a spelling suggestion.
- Source analysis producing byte-accurate spans, plus a comment scanner that in-source suppression
  directives will consume.
- Deterministic sharding by jump consistent hashing, so a nightly job can cover a different slice
  of a large workspace each night without losing prior coverage as the code evolves.
- `run`, `list` (`ops`, `mutants`, `files`) and `explain` subcommands, with human and JSON output.
- Cargo-style console progress: an animated arrowhead bar on stderr, weighted by estimated cost
  rather than mutant count so it does not stall at 99%.
- Mutant schemata: every selected mutant is compiled into one set of test binaries and chosen at
  run time by an environment variable, replacing the rebuild-per-mutant model.
- In-source suppression through three channels sharing one vocabulary: `#[gamma::skip(..)]`
  attributes, the identical form written as a `// #[gamma::skip(..)]` comment for positions where
  Rust does not yet accept attributes, and `#[mutants::skip]` for compatibility with
  `cargo-mutants`.
- Execution: scratch-tree copy, guard runtime vendoring, a baseline measurement that refuses to
  proceed against a failing suite, parallel per-mutant runs, and per-mutant timeouts derived from
  the baseline so a mutant that induces an infinite loop is cut off quickly.
- Automatic withdrawal of mutants that cannot compile. Compiler diagnostics are attributed to
  guards by line, the offending mutants are withdrawn, and the tree is rebuilt until it compiles;
  they are reported as unviable and excluded from the score rather than counted as survivors.
- Reports: a `mutation-testing-elements` JSON document via `--json-report`, and a single
  self-contained HTML page via `--html` that embeds both the viewer and the results, so it opens
  from a CI artifact or an air-gapped machine. `--html-external` loads the viewer from a CDN
  instead. A conformance test checks the emitted document against the vendored report schema, so
  upstream drift fails a build rather than producing a blank page in a browser.
- Configuration file at `.cargo/gamma.toml` covering everything expressible on the command line.
  Unknown keys are errors rather than settings that silently do nothing. Command-line scalars win;
  lists concatenate.
- `cargo gamma suppress`, which writes suppressions into the source for mutants that timed out or could
  not compile. A surviving mutant is never eligible and cannot be made eligible. Generated
  directives name the exact mutators and carry a tag, a reason and the date; after writing,
  discovery runs again and the edit is reverted in full unless the suppressed set changed by
  exactly the intended mutants and nothing else.
- `cargo gamma migrate`, which translates a `.cargo/mutants.toml` and a `cargo mutants` command
  line. Keys with no equivalent are preserved as `TODO` comments rather than dropped, and every
  translated line names the key it came from.
- `CARGO_GAMMA=1` in every test process a run launches, so a suite that drives cargo itself can
  step aside instead of failing the baseline from a nested build.
- `cargo gamma merge`, which combines the reports of a shard rotation into one score. Verdicts are
  unioned by content-addressed mutant ID and the most recent wins, so a nightly job that runs one
  shard a night still reports on the whole population. Because IDs move when their code changes, a
  mutant whose construct was edited appears as never tested rather than inheriting a verdict it
  never earned. Never-tested mutants stay out of the denominator, and the output reports freshness
  and which shards the rotation has yet to visit. `--window`, `--min-score`, `--json-report` and
  `--html` are accepted.
- `cargo gamma estimate`, which pays a run's fixed cost — build, baseline and the withdrawal of
  mutants that cannot compile — and then projects the rest rather than executing it. The same
  projection is printed automatically once the build and baseline are done, so a four-hour job is
  visible in the first minute instead of the fourth hour. It names the one quantity it had to
  assume, and reports a worst case alongside the estimate, because a CI job killed at the hour mark
  produces no report at all.
- `cargo gamma advise`, which diagnoses a finished run: a dominant build, a slow baseline, a run too
  long for a per-commit job, timeouts and their burned budget, unviable mutants, files holding an
  outsized share of the population, mutator families spending real time and finding nothing, and
  code no test reaches. Every finding states what its remedy costs in signal, exactly where that is
  computable. Share-based findings have absolute floors, so a toy project is not told that each of
  its two files is half the population.
- `--yields`, reporting cost and survivors found per mutator family.

- Hang detection calibrated from the baseline. A mutant whose test binary goes quiet for much longer
  than the healthy suite ever did is presumed hung and cut off, rather than waiting out a timeout
  derived from the whole suite — which is how a one-line change to a twelve-millisecond test comes
  to cost two minutes. The budget is measured, not fixed, so a suite whose slowest test takes half a
  minute is not accused of hanging every time it runs it. The verdict names the test it stalled in,
  and hung mutants are now listed explicitly instead of disappearing into the caught total, since a
  timeout counts as detected. `--no-stall-detection` restores the old behavior.

### Fixed

- Top-level `--help` introduced the tool by the library crate's package description, and listed
  every option of the implied `run` subcommand as though it were global — so `--ops` appeared to
  apply to `merge`. The implied `run` is now inserted during argument normalization instead of by
  flattening `RunArgs` into the top-level parser, so each help page describes only what that command
  accepts. A misspelled subcommand still reaches clap, and still gets its "did you mean".

- A test binary that printed more than a pipe holds deadlocked the run. Its output was piped but
  only read after the process exited, so a chatty binary blocked in `write` while the run waited for
  it to exit. The baseline stalled for ten minutes and then reported the wrong cause; a mutant was
  recorded as a timeout, and since a timeout counts as detected, a survivor could be silently
  scored as caught. This was not hypothetical: libtest dumps the captured output of every failing
  test, so any mutant that broke a talkative test hit it. Output is now drained on its own thread
  and capped, so only the first failure is kept. Dogfooding this repository went from a ten-minute
  stall to forty-five seconds, and the mutation score fell by eleven points once the false timeouts
  stopped counting as detections.

[Unreleased]: https://github.com/geeknoid/cargo-gamma/commits/main/
