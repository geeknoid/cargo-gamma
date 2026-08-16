# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Features
- [F2](#f2) — Checkpoint and resume long-running campaigns

### Performance
- [P1](#p1) — Preserve a delta-synchronized scratch workspace between campaigns
- [P2](#p2) — Reuse one in-memory workspace inventory within a campaign
- [P4](#p4) — Make instrumentation and rollback proportional to changed files
- [P5](#p5) — Compile package and feature relationships into one indexed graph
- [P6](#p6) — Index snapshot files instead of scanning them per mutant
- [P7](#p7) — Schedule workers and shards from measured mutant cost
- [P8](#p8) — Coordinate file-local killer learning across workers
- [P9](#p9) — Reduce census launches and compress repeated reach sets
- [P11](#p11) — Move stale-cgroup discovery out of the per-launch path
- [P12](#p12) — Store shared mutation sites once per source span
- [P13](#p13) — Fuse per-file AST analysis and parallelize declaration-only scans
- [P14](#p14) — Stream typed reports and reuse merge provenance
- [P15](#p15) — Compile selection filters for large file and diff sets
- [P16](#p16) — Evaluate a persistent per-file analysis cache across campaigns

### Benchmarks
- [B1](#b1) — Build a cross-repository mutation campaign corpus
- [B2](#b2) — Gate end-to-end performance on a generated large workspace
- [B3](#b3) — Measure warm campaign reuse and one-file deltas
- [B4](#b4) — Measure launch, census, scheduling, and containment scaling
- [B5](#b5) — Gate report and merge memory at large populations

## Features

<a id="f2"></a>
### F2 — Checkpoint and resume long-running campaigns

**Area:** execution coordinator and run records · **Priority:** High · **Effort:** Large

Persist completed work periodically so cancellation, interruption, or host restart loses a
bounded amount of a multi-day campaign. The coordinator thread should publish an explicitly
partial record atomically; workers must not contend on it. Throttle checkpoints by elapsed time
and completed-mutant count so short runs pay negligible overhead and long verdicts cannot prevent
a time-based checkpoint.

Persist completed verdict entries, exact killer probes, compiler-confirmed unviability, build
ordering data, and the pre-run workspace/context snapshots needed to validate them. Never infer a
verdict from an absent entry. Reuse partial entries under the same trust rules as completed
records: validated kills may settle, killer probes are verified, and survivors, timeouts, and
resource failures are rerun according to policy. `cargo gamma hints` should be able to promote
safe probe and build-order tiers from a partial record. Keep the last valid checkpoint if its
replacement is truncated, interrupted, or fails to sync, and keep final reports explicitly
incomplete until the population finishes.

**Done when:** interruption tests cover every publication boundary, an end-to-end resume test
proves that a partial record saves work without changing the final score, and the configured
checkpoint cadence places an explicit upper bound on progress at risk.

## Performance

<a id="p1"></a>
### P1 — Preserve a delta-synchronized scratch workspace between campaigns

**Area:** workspace preparation and Cargo incremental state · **Priority:** High · **Effort:** Large
**Evidence:** Structural · **Expected impact:** warm-run copy and rebuild time; potentially the
dominant setup saving, bounded by workspace preparation and recompilation · **Risk:** High:
stale files or links would invalidate results · **Scope:** one campaign-wide path, exhaustive

Every run deletes and recopies the scratch tree even though successful runs deliberately retain
the build directory for Cargo incremental reuse. Reflinked files are then assigned fresh mtimes,
which can invalidate fingerprints for unchanged inputs on a large workspace.

- `crates/cargo-gamma-lib/src/exec/workspace.rs:175` — `prepare` removes an existing scratch
  tree before copying the workspace again.
- `crates/cargo-gamma-lib/src/exec/workspace.rs:138` — successful runs retain build artifacts
  specifically to accelerate the next campaign.
- `crates/cargo-gamma-lib/src/exec/copy.rs:387` — every successful reflink calls `freshen`, which
  changes its modification time.

Retain a locked pristine scratch tree and delta-synchronize additions, removals, type changes,
symlinks, and changed contents. Restore only files cargo-gamma instrumented, preferably with
reflink copy-on-write. Do not hardlink user-writable files. An overlay backend is worth exploring
only where its cleanup and cross-platform semantics can be tested.

**Done when:** B3 shows that unchanged and one-file-delta campaigns touch and rebuild only the
necessary files without slowing cold preparation, or the approach is closed as not worth its
correctness and portability cost with those measurements recorded.

**See also:** B2, B3

---

<a id="p2"></a>
### P2 — Reuse one in-memory workspace inventory within a campaign

**Area:** discovery, incremental validation, and run records · **Priority:** High · **Effort:** Medium
**Evidence:** Structural · **Expected impact:** single-campaign startup latency and filesystem
traffic; bounded by discovery's phase share · **Risk:** Medium: snapshots taken at different
publication boundaries must not be conflated · **Scope:** all tree captures within one invocation,
exhaustive

A single invocation independently walks, reads, and hashes the same workspace for cache adoption,
the pre-run snapshot, killer discovery, mutation discovery, and record publication. These products
currently have separate owners even when they describe the same pre-execution bytes.

- `crates/cargo-gamma-lib/src/commands/run.rs:714` — cache adoption scans source for killer
  declarations before ordinary discovery.
- `crates/cargo-gamma-lib/src/commands/run.rs:793` — orchestration then captures another complete
  pre-run workspace snapshot and scans killers again.
- `crates/cargo-gamma-lib/src/discover/record.rs:675` — settling a prior record recaptures the
  workspace before validating its entries.

Build one immutable pre-run inventory containing path, type, size, mtime, and a lazily computed
digest, retain it in memory for the duration of the command, and pass it through adoption, copy
planning, discovery, and record construction. Capture one distinct post-run inventory only where
publication must prove the original workspace remained unchanged. Nothing from this item survives
process exit.

**Done when:** B3 demonstrates one pre-run and at most one post-run traversal per invocation,
workspace mutation and edit-then-revert tests preserve the existing trust boundaries, and the
change either lands with a worthwhile single-campaign wall-time or I/O reduction or is rejected
with that measurement recorded.

**See also:** B2, B3

<a id="p4"></a>
### P4 — Make instrumentation and rollback proportional to changed files

**Area:** source splicing and build convergence · **Priority:** High · **Effort:** Medium
**Evidence:** Structural · **Expected impact:** files read, copied, and rewritten across rollback
rounds; bounded by instrumentation and Cargo invalidation time · **Risk:** Medium: a stale guard
would produce an incorrect verdict · **Scope:** every convergence round, exhaustive

The splice cache avoids rewriting identical files, but every round still rebuilds the live
mutant-to-file grouping from the entire population and visits every planned file. Between rollback
rounds only files containing newly withdrawn mutants can have changed.

- `crates/cargo-gamma-lib/src/exec/build/splices.rs:22` — `by_file` scans all mutants on every
  call.
- `crates/cargo-gamma-lib/src/exec/build/splices.rs:74` — `instrument` loops over every plan file,
  computes live ordinals, and consults the cache.

Maintain a per-file live-ordinal index as part of the converger. Mark only files affected by newly
withdrawn ordinals dirty, and return cached guards for all other files without rebuilding their
grouping or ordinal vectors.

**Done when:** B2 varies 1, 10, and 100 rollback rounds and withdrawal rates; work scales with
dirty files rather than the whole population, withdrawn ordinals and diagnostics stay identical,
and the item may close as not worth the state complexity if the measured phase share is negligible.

**See also:** B2

---

<a id="p5"></a>
### P5 — Compile package and feature relationships into one indexed graph

**Area:** Cargo metadata interpretation and package reachability · **Priority:** Medium · **Effort:** Medium
**Evidence:** Structural · **Expected impact:** discovery CPU and memory at hundreds to thousands
of packages and features; bounded by graph preparation · **Risk:** Medium: fail-open handling of
opaque path dependencies must remain conservative · **Scope:** package closure, stages, and feature
propagation, exhaustive

Reachability performs a cloned-string depth-first search from every workspace member. Feature
resolution repeatedly rescans all members until a fixed point and clones each enabled-feature set
per pass.

- `crates/cargo-gamma-lib/src/discover/survey.rs:1030` — `reachable` builds string-keyed edges and
  computes a fresh transitive walk for every package.
- `crates/cargo-gamma-lib/src/cfg/features.rs:59` — feature propagation loops over the whole
  workspace until no entry changes.
- `crates/cargo-gamma-lib/src/cfg/features.rs:123` — every propagation pass clones a package's
  enabled feature set before traversing it.

Intern packages and features into integer IDs once, compute strongly connected components and
hybrid bitset reachability, and propagate newly enabled feature nodes through a work queue exactly
once. Reuse the graph for stage construction, test reachability, and package lookup.

**Done when:** B2 covers chain, dense, cyclic, opaque, and feature-heavy graphs up to 1,000
packages; edge visits approach linear growth, reach and enabled-feature sets remain identical, and
the change lands or is rejected based on measured discovery time and RSS.

**See also:** B2

---

<a id="p6"></a>
### P6 — Index snapshot files instead of scanning them per mutant

**Area:** incremental run-record construction · **Priority:** High · **Effort:** Small
**Evidence:** Structural · **Expected impact:** removes `O(mutants × files)` lookup growth;
bounded by run-record construction · **Risk:** Low · **Scope:** one lookup used for every settled
mutant, exhaustive

Workspace snapshots are sorted vectors, but `WorkspaceSnapshot::file` uses a linear scan. Record
construction calls it for each settled mutant, so a large population can multiply mutant count by
workspace file count.

- `crates/cargo-gamma-lib/src/discover/workspace_snapshot.rs:173` — `file` calls
  `self.files.iter().find`.
- `crates/cargo-gamma-lib/src/discover/record.rs:823` — record construction looks up the snapshot
  file for every settled mutant.

Use binary search on the existing sorted vector or maintain a path-to-index map built once with
the snapshot.

**Done when:** B3 varies files and mutants independently and shows lookup work no longer grows
multiplicatively with identical records, or closes the item with evidence that record
construction remains below measurement noise.

**See also:** B3

---

<a id="p7"></a>
### P7 — Schedule workers and shards from measured mutant cost

**Area:** sweep scheduling and CI sharding · **Priority:** High · **Effort:** Medium
**Evidence:** Reasoned · **Expected impact:** sweep and shard makespan, not total CPU; bounded by
current queue-tail and shard imbalance · **Risk:** Medium: stale estimates can create worse
ordering or shard churn · **Scope:** worker queue and shard assignment, exhaustive

Every mutant in a package receives the sum of the same whole-binary baselines as its expected cost,
although census data already records the duration of the tests that reach each site. Shards use a
stable hash that balances mutant counts rather than predicted work.

- `crates/cargo-gamma-lib/src/exec/sweep.rs:171` — scheduling estimates a mutant from all reachable
  binary baselines for its package.
- `crates/cargo-gamma-lib/src/exec/census.rs:172` — census can sum the measured duration of tests
  selected for an individual site.
- `crates/cargo-gamma-lib/src/discover/shard.rs:3` — jump-consistent hashing assigns shards without
  a workload weight.

Build a versioned cost model from selected-test duration, reachable-binary prefix, exact-killer
history, timeout frequency, and recent observed verdict latency. Use longest-expected-processing
time with aging for the local queue and an optional deterministic weighted shard manifest for CI.
Keep hash-only sharding as the compatibility fallback.

**Done when:** B4 replays 10,000 to 100,000 mixed-cost mutants and 8 to 64 shards; maximum shard
time and worker idle tail fall without changed verdicts, missing or duplicate mutants, or worse
no-history performance, or the model is rejected with those results recorded.

**See also:** B1, B4

---

<a id="p8"></a>
### P8 — Coordinate file-local killer learning across workers

**Area:** killer reuse and parallel sweep execution · **Priority:** Medium · **Effort:** Medium
**Evidence:** Structural · **Expected impact:** avoided test-binary prefixes for mutant-dense
files; bounded by files whose mutants share a killer and lack persisted hints · **Risk:** Medium:
waiting can idle workers when no common killer exists · **Scope:** all file-local learning,
exhaustive

One mutant becomes a file's learner. Workers that encounter siblings while learning is in
progress immediately run them without the eventual hint, so a large file can send most of its
mutants through the expensive cold path before the first killer is published.

- `crates/cargo-gamma-lib/src/exec/sweep.rs:649` — `Learning::InProgress` is returned as an error
  state rather than a waitable result.
- `crates/cargo-gamma-lib/src/exec/sweep.rs:668` — the learned killer is published only after the
  learner finishes its full judgement.

Compare bounded waiting, per-file queues, and one learner with a capped number of speculative
siblings. Wake waiters through a condition or event and feed the learned killer's duration into
P7's cost model.

**Done when:** B4 covers early, late, and absent common killers over 10 to 1,000 mutants per file;
ordinary launches and wall time fall without reducing utilization in the no-common-killer case,
or the current speculative behavior is retained with the measurements recorded.

**See also:** P7, B4

---

<a id="p9"></a>
### P9 — Reduce census launches and compress repeated reach sets

**Area:** test census and reachability matrix · **Priority:** High · **Effort:** Large
**Evidence:** Structural · **Expected impact:** census wall time and peak RSS on suites with many
small tests and sites; bounded to census-enabled campaigns · **Risk:** High: attribution must stay
exact or survivors become untrustworthy · **Scope:** all census tasks and retained reach data,
exhaustive

The census launches one process per test, materializes every `(binary, test)` task, protects one
site map per binary with a shared mutex, and stores a separate `Vec<u32>` for every reached site.
Many sites commonly share the same test set.

- `crates/cargo-gamma-lib/src/exec/census.rs:518` — process isolation is the mechanism used to
  attribute sites to one test.
- `crates/cargo-gamma-lib/src/exec/census.rs:579` — the complete task product is collected before
  workers start.
- `crates/cargo-gamma-lib/src/exec/census.rs:592` — per-binary reached maps are shared behind
  mutexes.
- `crates/cargo-gamma-lib/src/exec/census.rs:647` — each reached site appends into that shared map.

First move to worker-local reach maps and lazy task indexing, then merge once. Intern identical
sorted reach sets or compare compact bitmap representations. Explore grouped attribution only with
a protocol that can prove which test emitted each site; retain one-process-per-test as the
correctness fallback.

**Done when:** B4 scales from 100 to 100,000 tests and 10,000 to 1,000,000 sites; launches, lock
wait, allocations, RSS, and wall time improve while decoded reachability is byte-for-byte equal,
or each technique is closed with the measurement that rejected it.

**See also:** B4

---

<a id="p11"></a>
### P11 — Move stale-cgroup discovery out of the per-launch path

**Area:** Linux memory accounting and process containment · **Priority:** High · **Effort:** Medium
**Evidence:** Structural · **Expected impact:** metered launch latency and syscall count for short
tests; negligible for long tests · **Risk:** High: cleanup must never remove a live foreign
invocation's cgroup · **Scope:** every Linux metered process launch, exhaustive

Creating each cgroup first scans the delegated root, inspects every candidate leaf, checks process
generations, and probes shared reaper state. Healthy launches therefore pay a directory-wide stale
cleanup cost even when no stale leaf exists.

- `crates/cargo-gamma-unsafe/src/cgroup.rs:195` — `reap_owned_leaves` walks the entire delegated
  root and checks each leaf owner.
- `crates/cargo-gamma-unsafe/src/cgroup.rs:534` — `Cgroup::create` calls that scan before every
  leaf creation.

Move stale-leaf discovery to boundary initialization and the background reaper, maintain pending
state incrementally, and keep foreground creation proportional to one new leaf. Preserve
generation checks and registration under the existing critical section.

**Done when:** B4 shows metered launch throughput remains flat from zero to 100,000 stale or
foreign leaves, all containment and cleanup tests remain unchanged, and the redesign lands or is
rejected based on its measured launch share.

**See also:** B4

---

<a id="p12"></a>
### P12 — Store shared mutation sites once per source span

**Area:** mutant population representation · **Priority:** High · **Effort:** Large
**Evidence:** Structural · **Expected impact:** peak RSS, hashing, and report bytes approximately
proportional to repeated replacements per site; CPU gain bounded by population finalization and
serialization · **Risk:** High: identity and interchange compatibility are correctness-sensitive
· **Scope:** all candidate-to-definition conversion, exhaustive

Several mutators emit multiple replacements for one source span, but every resulting definition
stores and normalizes the original text, computes source locations, and carries repeated path,
item, mutator, and site data. A second population-wide interning pass then rewrites shared symbols.

- `crates/cargo-gamma-engine/src/ops/collect/definitions.rs:24` — every candidate copies its
  original source and normalizes it independently.
- `crates/cargo-gamma-engine/src/ops/collect/definitions.rs:33` — every replacement repeats source
  location lookup for the same span.
- `crates/cargo-gamma-engine/src/ops/collect/definitions.rs:43` — each definition retains the
  repeated site fields alongside its replacement.

Introduce an internal site table keyed by file and span, containing original text, normalized
identity material, location, and item symbol. Store compact site, file, package, item, and mutator
IDs in each replacement and expand them only at public report boundaries.

**Done when:** B2 measures bytes per retained mutant, allocations, peak RSS, collection throughput,
and stable IDs at 100,000 or more mutants; the representation lands with a worthwhile memory win
and no throughput regression, or is closed as not worth the API complexity with those numbers.

**See also:** B2, B5

---

<a id="p13"></a>
### P13 — Fuse per-file AST analysis and parallelize declaration-only scans

**Area:** source parsing and mutation collection · **Priority:** Medium · **Effort:** Large
**Evidence:** Structural · **Expected impact:** selected-file AST memory traffic and narrow-scan
latency; cannot remove `syn` parsing itself · **Risk:** Medium-high: fused visitors couple
selection, diagnostics, indexes, and workspace facts · **Scope:** every parsed source file and all
filter-excluded declaration files, exhaustive

Selected ASTs receive independent visitors for workspace defaults, stated-value diagnostics,
selection-dependent indexes, and candidate collection. Files excluded by `--file` or `--in-diff`
are then fully parsed for module declarations in a serial tail after parallel selected-file work.

- `crates/cargo-gamma-engine/src/ops/collect/collector/indexes.rs:291` — a dedicated visitor builds
  numeric and import indexes before collection.
- `crates/cargo-gamma-engine/src/ops/collect/traversal.rs:42` — the collector performs another full
  AST traversal.
- `crates/cargo-gamma-lib/src/discover/survey.rs:806` — declaration-only files are read and parsed
  serially after worker collection.

Have the phase-one visitor emit workspace defaults, stated diagnostics, and per-file indexes
together, retaining the stateful source-order collector as a separate pass. Put declaration-only
files into the existing dynamic work queue and evaluate a lexer proof that can skip `syn` when no
relevant `mod` token exists.

**Done when:** B2 attributes node visits, parse count, wall time, allocations, and peak RSS for
large files and narrow selections; each fusion or prefilter lands only with identical declarations
and mutants plus a worthwhile measured win, otherwise it is rejected explicitly.

**See also:** P2, B2

---

<a id="p14"></a>
### P14 — Stream typed reports and reuse merge provenance

**Area:** report serialization and shard merging · **Priority:** Medium · **Effort:** Large
**Evidence:** Structural · **Expected impact:** report and merge peak RSS, allocation traffic, and
hashing at hundreds of megabytes; no mutation-execution speedup · **Risk:** High: schema validation
and deterministic lineage must remain exact · **Scope:** all report reads, writes, and merges,
exhaustive

Report writing first materializes a complete `serde_json::Value` before streaming it. Merge reading
holds source text, that owned value, and a typed report concurrently. Merge then serializes each
mutant again to derive lineage, clones a complete base report, and reclones winning source and
mutant payloads.

- `crates/cargo-gamma-lib/src/elements/report.rs:904` — `write_json` converts the full report to an
  owned `Value` before writing.
- `crates/cargo-gamma-lib/src/merge/read.rs:94` — input text is decoded to `Value`, schema
  validated, and converted again to `Incoming`.
- `crates/cargo-gamma-lib/src/merge/union.rs:386` — verdict lineage serializes every mutant to a
  fresh byte vector.
- `crates/cargo-gamma-lib/src/merge/union.rs:446` — rebuilding starts by cloning the entire base
  report.

Validate typed construction directly or through a non-owning serialization visitor, deserialize
once into a validation-aware typed shape, compute provenance once into indexed metadata, and build
the merged report directly from winners. Keep the current implementation as an oracle during the
transition.

**Done when:** B5 shows materially lower peak RSS and allocation counts on 256–512 MiB inputs,
accepted and rejected schema corpora produce equivalent diagnostics, staged and permuted merges
remain equivalent, and the redesign lands or is rejected with those measurements.

**See also:** B5

---

<a id="p15"></a>
### P15 — Compile selection filters for large file and diff sets

**Area:** file filtering and diff-scoped mutation · **Priority:** Medium · **Effort:** Small
**Evidence:** Structural · **Expected impact:** allocation and selection CPU proportional to
files × patterns and mutants × changed lines; zero for unfiltered runs · **Risk:** Low-medium:
Unicode and platform path semantics must remain identical · **Scope:** all glob and diff overlap
queries, exhaustive

Every glob match renormalizes strings, tokenizes the pattern, converts the path to characters, and
allocates two dynamic-programming rows. Every mutant selected by a diff linearly scans every
changed line in its file.

- `crates/cargo-gamma-lib/src/discover/glob.rs:13` — normalization and tokenization occur inside
  every match call.
- `crates/cargo-gamma-lib/src/discover/glob.rs:48` — each match allocates a character vector and two
  boolean vectors.
- `crates/cargo-gamma-lib/src/discover/diff.rs:242` — overlap uses `lines.iter().any` for each
  mutation site.

Compile normalized glob token programs once and reuse per-worker DP buffers. Sort and deduplicate
changed lines or coalesce them into intervals, then answer overlap by binary search.

**Done when:** B2 varies paths × patterns and mutants × changed lines, allocations and lookup time
scale sublinearly in the second dimension, cross-platform matching stays identical, and each
optimization lands or is rejected with its measurement.

**See also:** B2

---

<a id="p16"></a>
### P16 — Evaluate a persistent per-file analysis cache across campaigns

**Area:** mutation discovery and on-disk cache · **Priority:** Low · **Effort:** Large
**Evidence:** Speculative · **Expected impact:** repeated-campaign parsing time only; zero benefit
to a cold campaign and potentially negative if cache I/O rivals parsing · **Risk:** High: cache
invalidation, schema evolution, corruption handling, and disk growth add substantial complexity
· **Scope:** every selected Rust file across separate cargo-gamma invocations, exhaustive

Every campaign rereads and reparses selected Rust files even when their contents and analysis
context are unchanged. Persisting parse-derived products could avoid that work across invocations,
but serializing, reading, validating, and invalidating those products may cost as much as parsing
the source and would introduce a brittle cache whose correctness depends on a complete key.

- `crates/cargo-gamma-lib/src/discover/survey.rs:916` — each campaign reads and parses every
  selected file before building defaults, indexes, declarations, and mutant definitions.
- `crates/cargo-gamma-engine/src/parse/source_file.rs:41` — parsing also builds line starts,
  comments, nesting information, and a complete `syn` syntax tree.

Before designing a durable format, prototype the smallest useful cache product and compare its
write, read, validation, and invalidation cost against direct reparsing. A candidate key must cover
source content, cargo-gamma and identity-format versions, effective `CfgSet`, mutator selection,
and workspace facts used by collection. Corrupt, unknown, or incomplete entries must be discarded
and reparsed rather than weakening the population. Do not persist a `syn` AST unless its format and
compatibility costs are demonstrably better than source parsing.

**Done when:** B3 separately measures cold cache creation, unchanged warm lookup, one-file
invalidation, whole-cache version invalidation, and corrupt-entry recovery; implement persistent
reuse only if warm wall time improves enough to justify its disk and maintenance cost with
byte-identical ordered mutant identities, otherwise close this item with the measurements that
show reparsing is preferable.

**See also:** P2, B3

## Benchmarks

<a id="b1"></a>
### B1 — Build a cross-repository mutation campaign corpus

**Area:** mutation quality and campaign performance · **Priority:** High · **Effort:** Large

Repeat the initial Exemel analysis across codebases with materially different shapes: CLI,
async/server, proc macro, `no_std`, numeric, parser/serializer, and application code. A single
campaign is the first data point, not a basis for default mutator policy.

Retain and compare, by mutator:

- generated, viable, unviable, killed, survived, timeout, out-of-memory, and uncovered counts;
- rollback rounds and build time attributable to each unviable family;
- baseline duration, verdict latency, and resource use by test binary;
- killer concentration and whether kills are focused or incidental to broad smoke tests;
- unique survivors and killers contributed by each mutator or family;
- syntax and type context for unviable and low-value sites.

For the Exemel follow-up, recompute these tables from the final report and diagnostics bundle;
separate equivalent/debug-only survivors from genuine missing assertions; correlate timeout and
out-of-memory verdicts with target, mutator, and syntax context; compare a representative shard
under an optimized Cargo `gamma` profile; isolate performance-only checks from broad oracle targets
where correctness detection remains; apply only collector heuristics guarded by positive and
negative tests; rerun the same shard; and then repeat the experiment elsewhere before changing
preset membership.

**Done when:** the corpus contains the named workload shapes, preserves reproducible raw campaign
artifacts and normalized comparison tables, and supports further collector-suppression and
pedantic-preset decisions without generalizing from one repository.

---

<a id="b2"></a>
### B2 — Gate end-to-end performance on a generated large workspace

**Area:** complete campaign and phase attribution · **Priority:** High · **Effort:** Large

There is no benchmark target or CI performance gate for the paths cargo-gamma documents as
expensive. Build a deterministic workspace generator covering a 500-package, 5,000-file,
approximately two-million-line workspace with at least 100,000 mutants. Include dependency chains,
fan-out, cycles, independent packages, feature-heavy packages, overlapping target roots, large
files, and narrow file/diff selections.

Measure wall time, peak RSS, bytes read and written, files parsed, AST passes, Cargo invocations,
compile units rebuilt, instrumentation rounds, and retained bytes per mutant. Record phase shares
so Amdahl's law can distinguish discovery, build, census, execution, and reporting wins.

- `crates/cargo-gamma-lib/src/commands/run.rs:779` — the run orchestrator spans discovery,
  workspace capture, preparation, execution, and publication.
- `crates/cargo-gamma-lib/src/exec/measure.rs:631` — staged scanning and compilation are the main
  large-workspace preparation loop.
- `.github/workflows/main.yml:1` — CI runs correctness checks but no performance target.

**Done when:** a dedicated stable runner executes at least five samples, stores raw phase results,
and fails median wall-time regressions above 10% or peak-RSS regressions above 15%; population,
withdrawal, and verdict equivalence remain hard correctness gates.

**See also:** P4, P5, P12, P13, P15

---

<a id="b3"></a>
### B3 — Measure warm campaign reuse and one-file deltas

**Area:** workspace copy, snapshots, and incremental discovery · **Priority:** High · **Effort:** Medium

Use B2's workspace in cold, unchanged-warm, one-Rust-file edit, manifest edit, build-script input,
external path dependency, add/remove/rename, symlink, and edit-then-revert scenarios. Run on
reflink-capable and fallback filesystems.

Measure entries walked, metadata calls, bytes read and hashed, files copied/reflinked/touched,
Rust files parsed, cache hits, compile units rebuilt, wall time, and peak RSS. This is both an
improvement baseline and a maintenance gate for the existing retained target directory and reflink
fast path.

- `crates/cargo-gamma-lib/src/exec/workspace.rs:175` — workspace preparation owns the scratch copy
  and retained build directory.
- `crates/cargo-gamma-lib/src/discover/record.rs:675` — incremental adoption recaptures and hashes
  inputs.

**Done when:** CI fails warm wall-time or RSS regressions above 10%, unchanged runs rebuild no
unaffected compile units, and every invalidation scenario proves the cached and uncached mutant
populations and verdicts identical.

**See also:** P1, P2, P6, P16

---

<a id="b4"></a>
### B4 — Measure launch, census, scheduling, and containment scaling

**Area:** repeated mutant execution · **Priority:** High · **Effort:** Large

Create deterministic campaign fixtures with 10,000, 50,000, and 100,000 mutants; 100 to 100,000
tests; 10,000 to 1,000,000 census sites; early, late, and absent common killers; empty, 1 ms, 10 ms,
and long tests; and 1 to 64 workers. Cover direct libtest and nextest on Linux and Windows, plus
delegated Linux cgroups with zero to 100,000 stale or foreign leaves.

Measure launch throughput and p95 startup latency, worker utilization and idle tail, queue-cost
prediction error, census launches, lock wait, reach-map allocations and RSS, cgroup syscalls,
reader threads, shard max/median duration, and final wall time.

- `crates/cargo-gamma-lib/src/exec/sweep.rs:333` — workers consume the static mutant queue through
  one atomic cursor.
- `crates/cargo-gamma-lib/src/exec/census.rs:518` — census attribution launches one process per
  test.
- `crates/cargo-gamma-lib/src/exec/verdict.rs:690` — every attempt prepares and spawns a new
  process tree.

**Done when:** a stable runner fails wall-time, startup-latency, utilization, or RSS regressions
above 10%; exact reach maps, verdicts, process isolation, shard completeness, and containment
cleanup are hard gates rather than performance tolerances.

**See also:** P7, P8, P9, P11

---

<a id="b5"></a>
### B5 — Gate report and merge memory at large populations

**Area:** report serialization and merge · **Priority:** Medium · **Effort:** Medium

Generate reports with 10,000 to 1,000,000 mutants, 1 to 500 MiB of embedded source, and 1, 100, and
4,096 shards. Include overlapping and divergent source generations, legacy provenance, long
replacement strings, invalid schema cases, and staged/permuted merge orders.

Measure peak RSS, allocations, bytes hashed and serialized, parse and write throughput, and total
artifact bytes. This protects publication and merge from becoming the memory ceiling after
execution is optimized.

- `crates/cargo-gamma-lib/src/elements/report.rs:904` — report writing owns a full JSON value.
- `crates/cargo-gamma-lib/src/merge/read.rs:19` — report reading documents three concurrent live
  representations.
- `crates/cargo-gamma-lib/src/merge/union.rs:386` — merge lineage hashes serialized mutant
  payloads.

**Done when:** a stable runner fails peak-RSS or throughput regressions above 10%, schema
acceptance and diagnostics match the committed corpus, and staged and permuted merges remain
equivalent.

**See also:** P12, P14
