# Configuration

Everything expressible on the command line is expressible in `.cargo/gamma.toml`, so a project that
has settled on a set of options does not have to repeat it in every CI job and every developer's
shell history.

* [Where the file lives](#where-the-file-lives)
* [Precedence](#precedence)
* [Every key](#every-key)
* [Coming from cargo-mutants](#coming-from-cargo-mutants)

## Where the file lives

`.cargo/gamma.toml`, relative to the directory being analyzed. Two flags override that:

```bash
cargo gamma run --config ci/gamma.toml   # read this file instead
cargo gamma run --no-config              # read nothing
```

An explicit `--config` path must exist. Asking for a file and silently getting the defaults because
the path was misspelled is exactly the failure that check is there to prevent, whereas a missing
conventional file is the ordinary case and is not an error.

**Unknown keys are errors.** A configuration file whose settings are silently ignored is worse than
no configuration file at all, because the project believes it is configured. A misspelled key, or a
key for a feature this build does not have, stops the run and names the offender.

## Precedence

A flag given on the command line wins over the file; the file wins over the built-in default. For
the list-valued keys — `files`, `packages`, `errors` and the rest — the two **concatenate**, so
adding one exclusion at the command line does not silently drop the ones the project agreed on. Use
`--no-config` for a run that should ignore what is checked in entirely.

```toml
# .cargo/gamma.toml
ops = [
    "@all",
    "!stmt",   # too many equivalent mutants in the parser
]
exclude-files = ["src/generated/**"]
min-score = 80.0
```

## Every key

All keys are optional. A file that sets one key is a valid file.

### Selecting what to mutate

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `ops` | list of strings | `--ops` | The mutator selector list. A list rather than one string, so each selector can sit on its own line with a comment saying why. Entries are joined with commas and parsed by the same code as the flag, so the two cannot drift. See [operators](OPERATORS.md) and [profiles](PROFILES.md). |
| `files` | list of globs | `--file` | Only mutate files matching one of these. |
| `exclude-files` | list of globs | `--exclude-file` | Never mutate files matching one of these. |
| `packages` | list of names | `-p`, `--package` | Packages to mutate. Empty means every package in the workspace. |
| `errors` | list of expressions | `--error` | Additional `Err(...)` values for `fn_value.err_with`, one mutant each, on every function returning a `Result`. |

### Choosing the tests that decide a verdict

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `test-packages` | list of names | `--test-package` | Packages whose tests decide a verdict. Empty means whichever package can reach the mutant. |
| `include-tests` | list of globs | `--include-test` | Test target names whose tests may decide a verdict. Empty means all of them. |
| `exclude-tests` | list of globs | `--exclude-test` | Test target names whose tests must not decide a verdict. Excluding a test makes mutants *harder* to kill, not easier. |
| `no-baseline` | boolean | `--no-baseline` | Skip the baseline measurement. Faster, at the cost of the timeouts being guesses rather than measurements, and of never learning that the suite was already failing. |

### Building

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `features` | list of strings | `--features` | Cargo features to activate. |
| `all-features` | boolean | `--all-features` | Activate every feature of every selected package. |
| `no-default-features` | boolean | `--no-default-features` | Do not activate the `default` feature. |
| `profile` | string | `--profile` | The cargo profile to build with. |
| `cargo-args` | list of strings | `-C`, `--cargo-arg` | Extra arguments for every cargo invocation. |
| `cargo-test-args` | list of strings | `--cargo-test-arg` | Extra arguments for every test binary, after the `--`. |

### Time

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `timeout` | seconds | `--timeout` | A fixed per-mutant timeout. Setting this replaces the measured one entirely. |
| `timeout-multiplier` | number | `--timeout-multiplier` | The multiple of the measured baseline duration a mutant is allowed before it counts as killed by timeout. |
| `minimum-test-timeout` | seconds | `--minimum-test-timeout` | A lower bound on the per-mutant timeout, so a suite that runs in milliseconds does not get a timeout measured in milliseconds. |
| `build-timeout` | seconds | `--build-timeout` | A fixed build timeout. |
| `build-timeout-multiplier` | number | `--build-timeout-multiplier` | The multiple of the first build's duration a later build round is allowed. |

A mutant that removes a loop's exit condition does not fail its tests, it never finishes them, so a
timeout has to be a verdict rather than an error. Timeouts are derived from the baseline by default;
the keys above only adjust that.

### Memory

Memory control is the same idea applied to allocation: a mutant that removes a bound can make a test
allocate without limit, which on an unprotected machine takes the whole run down with it rather than
producing a verdict.

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `memory` | `off`, `measure`, `enforce` | `--memory` | How much memory control to place around each test binary. `measure` records peak usage without limiting; `enforce` also imposes the ceiling and reports a mutant that breaches it as killed, shown as `OUTOFMEM`. |
| `memory-multiplier` | number | `--memory-multiplier` | The multiple of a test binary's baseline peak memory a mutant of it may reach. |
| `memory-headroom` | size | `--memory-headroom` | Absolute headroom added on top of the baseline peak, as a size such as `128MiB`. |
| `memory-limit` | size | `--memory-limit` | An explicit ceiling for every test binary, as a size such as `2GiB`, replacing the measured one. |
| `baseline-memory-limit` | size | `--baseline-memory-limit` | A ceiling for the baseline runs themselves, as a size such as `4GiB`. |

Sizes accept `KiB`, `MiB`, `GiB` and their decimal spellings.

### Running

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `jobs` | number | `--jobs` | How many mutants to test at once. Defaults to the machine's parallelism. |
| `min-score` | percentage | `--min-score` | Fail the run below this mutation score. This is what makes a run a gate rather than a report. |

### `[shard]`

Splitting one run across several CI machines. Each shard runs a disjoint slice of the mutants, and
the slices are assigned by hashing, so adding a mutant does not reshuffle every other one across
shards and invalidate a comparison.

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `count` | number | `--shard-count` | How many shards to divide the mutants into. |
| `index` | number | `--shard-index` | Which shard this run is, counted from zero. |

```toml
[shard]
count = 4
index = 0
```

### `[reporters]`

Where to write file reports. Each is off unless a path is given.

| Key | Type | Flag | Meaning |
| --- | --- | --- | --- |
| `html` | path | `--html` | A self-contained HTML report, viewable straight from a CI artifact with no server. |
| `json` | path | `--json-report` | A `mutation-testing-elements` JSON report, the interchange format the wider mutation-testing ecosystem reads. |
| `html-external` | boolean | `--html-external` | Load the HTML viewer from a CDN instead of embedding it, for a much smaller file that needs network access to open. |
| `sarif` | path | `--sarif` | A SARIF log of surviving mutants, which GitHub renders as annotations on the pull request that introduced them. |

```toml
[reporters]
html = "target/gamma/report.html"
sarif = "target/gamma/gamma.sarif"
```

## Coming from cargo-mutants

`.cargo/mutants.toml` is never read. It is a different schema for a different tool with a different
mutant catalog, so honouring it silently would mean another tool's `exclude_re` entries quietly
changing which mutants this one suppresses, and therefore quietly changing the score.

Translate it once, explicitly:

```bash
cargo gamma migrate --dry-run   # print the translation
cargo gamma migrate             # write .cargo/gamma.toml
```

Nothing is dropped: a key that cannot be expressed here becomes a `TODO` comment carrying the
original text verbatim, and every translated line is annotated with the key it replaces, so the
output can be read by someone who knows the old file and not the new one. An existing `gamma.toml`
stops the migration rather than being overwritten.
