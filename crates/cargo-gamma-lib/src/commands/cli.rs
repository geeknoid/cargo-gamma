use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::ci::{Annotations, Level};
use crate::error::error;
use crate::ops::registry::Selection;

use super::When;

/// Adapts a [`crate::bounds`] check to clap's parser signature.
macro_rules! bounded {
    ($name:ident) => {
        fn $name(text: &str) -> Result<f64, String> {
            let value: f64 = text.parse().map_err(|_cause| format!("`{text}` is not a number"))?;

            crate::bounds::$name(text, value)
        }
    };
}

bounded!(seconds);
bounded!(factor);
bounded!(percentage);

/// Adapts the memory-size check to clap's parser signature.
fn size(text: &str) -> Result<u64, String> {
    crate::bounds::size(text)
}

/// Fast mutation testing for Rust.
///
/// With no subcommand, `run` is implied by argument normalization rather than by flattening
/// `RunArgs` here, so each help page lists only its own options.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-gamma",
    bin_name = "cargo gamma",
    version,
    about = "Fast mutation testing for Rust.",
    long_about = "Fast mutation testing for Rust.\n\nEvery selected mutant is compiled into one \
                  set of test binaries and chosen at run time, so a whole workspace is mutated \
                  without rebuilding it once per mutant.\n\nWith no subcommand, `run` is implied.",
    max_term_width = 100
)]
pub struct Cli {
    /// The subcommand to run. Defaults to `run`.
    #[command(subcommand)]
    pub command: Command,

    /// When to use color in output.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub color: When,

    /// When to show the progress display.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub progress: When,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run mutation testing.
    Run(RunArgs),

    /// List what would be done, without doing it.
    List(ListArgs),

    /// Explain a mutator, a mutant, or a suppression.
    Explain(ExplainArgs),

    /// Translate a cargo-mutants project to cargo-gamma.
    Migrate(MigrateArgs),

    /// Write suppressions into the source for mutants that cannot usefully be tested.
    Suppress(SuppressArgs),

    /// Combine per-shard reports into one answer.
    Merge(MergeArgs),

    /// Print a shell completion script.
    Completions(CompletionsArgs),
}

/// Arguments for `merge`.
#[derive(Debug, Args)]
pub struct MergeArgs {
    /// The reports to merge. A directory is read for its `*.json` files.
    #[arg(value_name = "REPORTS", required = true)]
    pub inputs: Vec<Utf8PathBuf>,

    /// Write the merged `mutation-testing-elements` document here.
    #[arg(long, value_name = "PATH")]
    pub json_report: Option<Utf8PathBuf>,

    /// Write a self-contained merged HTML report here.
    #[arg(long, value_name = "PATH")]
    pub html: Option<Utf8PathBuf>,

    /// Days after which a verdict is reported as stale.
    ///
    /// Stale verdicts are still counted. Dropping them would shrink the denominator, which raises
    /// the score by forgetting rather than by testing.
    #[arg(long, value_name = "DAYS", default_value = "30")]
    pub window: u64,

    /// Fail if the merged score is below this percentage.
    ///
    /// Score gates belong here rather than on a shard run: a shard's own score moves by a third of
    /// a point per survivor, so a threshold set on one fires on noise.
    #[arg(long, value_name = "PERCENT", value_parser = percentage)]
    pub min_score: Option<f64>,
}

/// Arguments for `suppress`.
#[derive(Debug, Args)]
pub struct SuppressArgs {
    /// The run to perform before writing anything.
    #[command(flatten)]
    pub run: RunArgs,

    /// Print the diff without changing anything.
    ///
    /// Spelled apart from the run's own `--dry-run`, which stops before building at all: this one
    /// runs everything and holds back only the source edit.
    #[arg(long)]
    pub dry_run_suppress: bool,

    /// Which verdicts may be suppressed.
    ///
    /// A surviving mutant is never eligible and cannot be made eligible: it is a real gap in the
    /// test suite, and suppressing it would remove the gap from the score rather than from the code.
    #[arg(long, value_name = "LIST", default_value = "timeout")]
    pub eligible: String,
}

/// Arguments for `migrate`.
#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Path to the project to migrate.
    #[arg(short = 'd', long, value_name = "PATH", default_value = ".")]
    pub dir: Utf8PathBuf,

    /// Print the translation without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Translate a `cargo mutants` command line instead of a configuration file.
    ///
    /// Everything after this flag is treated as the command to translate, so the invocation from a
    /// CI workflow can be pasted in unquoted.
    #[arg(long, value_name = "ARG", num_args = 1.., allow_hyphen_values = true, conflicts_with = "dry_run")]
    pub command: Vec<String>,
}

/// Arguments shared by commands that select mutants.
#[derive(Debug, Args, Clone)]
pub struct SelectArgs {
    /// Path to the workspace or package to analyze.
    #[arg(short = 'd', long, value_name = "PATH", default_value = ".")]
    pub dir: Utf8PathBuf,

    /// Mutators to apply, as a comma-separated selector list.
    ///
    /// A selector is a mutator name (`arith.add_to_sub`), a family (`relational`), a profile
    /// (`@arithmetic`), or `all`. Prefix a selector with `!` to remove it from the set. Selectors
    /// are applied left to right, so `@arithmetic,!bitwise` means what it reads as.
    #[arg(long, value_name = "SELECTORS", allow_hyphen_values = true)]
    pub ops: Option<String>,

    /// Only mutate files matching these glob patterns.
    #[arg(long = "file", value_name = "GLOB")]
    pub files: Vec<String>,

    /// Skip files matching these glob patterns.
    #[arg(long = "exclude-file", value_name = "GLOB")]
    pub exclude_files: Vec<String>,

    /// Number of shards to divide the mutants into.
    #[arg(long, value_name = "N", requires = "shard_index")]
    pub shard_count: Option<u32>,

    /// Which shard to run, from 0.
    #[arg(long, value_name = "I", requires = "shard_count")]
    pub shard_index: Option<u32>,

    /// Only mutate lines added or changed by this unified diff, or `-` for standard input.
    ///
    /// This is what makes a run affordable on a pull request: the population is restricted to the
    /// code under review rather than sampled from the whole tree, so the result speaks about the
    /// change. Sharding is not a substitute, since a shard is a slice of everything.
    #[arg(short = 'D', long, value_name = "PATH")]
    pub in_diff: Option<Utf8PathBuf>,

    /// Only mutate these packages. Defaults to every package in the workspace.
    #[arg(short = 'p', long = "package", value_name = "NAME")]
    pub packages: Vec<String>,

    /// Mutate every package in the workspace.
    ///
    /// Accepted for symmetry with cargo and with `--package`; it is already the default.
    #[arg(long, conflicts_with = "packages")]
    pub workspace: bool,

    /// Additional values for `fn_value.err_with`, which replaces a function body with `Err(...)`.
    ///
    /// `fn_value.err_default` only reaches error types that implement `Default`. Naming a value
    /// here — `--error 'std::io::Error::from(std::io::ErrorKind::Other)'` — reaches the rest.
    #[arg(long = "error", value_name = "EXPR")]
    pub errors: Vec<String>,

    /// Which cargo features to build with.
    #[command(flatten)]
    pub features: FeatureArgs,

    /// Where the configuration comes from.
    #[command(flatten)]
    pub config: ConfigArgs,
}

/// Cargo feature selection, shared by discovery and the build.
///
/// Discovery and the build must agree: finding files under one feature set and compiling under
/// another produces guards that are not in the compiled tree.
#[derive(Debug, Args, Clone, Default)]
pub struct FeatureArgs {
    /// Cargo features to activate, comma-separated or repeated.
    #[arg(long = "features", value_name = "FEATURES")]
    pub features: Vec<String>,

    /// Activate every feature of every selected package.
    #[arg(long, conflicts_with = "no_default_features")]
    pub all_features: bool,

    /// Do not activate the `default` feature.
    #[arg(long)]
    pub no_default_features: bool,
}

/// Where the configuration file comes from.
#[derive(Debug, Args, Clone, Default)]
pub struct ConfigArgs {
    /// Read configuration from this file instead of `.cargo/gamma.toml`.
    #[arg(long = "config", value_name = "FILE", conflicts_with = "no_config")]
    pub path: Option<Utf8PathBuf>,

    /// Ignore the configuration file entirely.
    ///
    /// Without this there is no way to script a run that is independent of whatever the project
    /// happens to have committed.
    #[arg(long)]
    pub no_config: bool,
}

/// How long a build may take before it is abandoned.
///
/// Not offered to `estimate`. These change nothing an estimate reports — they can only turn a
/// working estimate into an error — and capping the build is at odds with a subcommand whose job is
/// to tell you what the build costs.
#[derive(Debug, Args, Default)]
pub struct BuildLimitArgs {
    /// Seconds the build may take before the run is abandoned.
    ///
    /// A run builds once, so a build that never finishes costs everything rather than one mutant.
    #[arg(long, value_name = "SECONDS", value_parser = seconds, conflicts_with = "build_timeout_multiplier")]
    pub build_timeout: Option<f64>,

    /// Multiple of the first successful build's duration that a later build round is allowed.
    ///
    /// Rollback rounds rebuild the same tree with fewer mutants, so a round that runs far longer
    /// than the first one is not making progress.
    #[arg(long, value_name = "FACTOR", value_parser = factor)]
    pub build_timeout_multiplier: Option<f64>,

    /// How many times the tree may be rebuilt while withdrawing mutants that do not compile.
    ///
    /// A mutant like `Some(Default::default())` only compiles when the type happens to implement
    /// `Default`, and rustc reports only the errors it reaches before it stops, so a large tree can
    /// need many rounds to converge. Raise this when a run stops with a rollback-limit error and the
    /// withdrawal counts it prints are still falling.
    #[arg(long, value_name = "ROUNDS", default_value_t = crate::exec::DEFAULT_ROLLBACK_ROUNDS)]
    pub rollback_rounds: u32,
}


/// The options common to every command that builds, measures a baseline and runs tests.
///
/// Shared by `run`, `estimate` and `advise`, because all three build the tree and measure the
/// baseline the same way — an estimate that measured differently from the run it predicts would be
/// predicting a different run.
#[derive(Debug, Args, Default)]
pub struct MeasureArgs {
    /// How many mutants to test at once. Defaults to the number of available cores.
    #[arg(short = 'j', long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Seconds a single mutant may run before it is called a timeout.
    ///
    /// By default this is derived from how long the baseline suite takes, which adapts to the
    /// machine and to the suite instead of guessing.
    #[arg(long, value_name = "SECONDS", value_parser = seconds)]
    pub timeout: Option<f64>,

    /// Multiple of the baseline duration a mutant is allowed.
    #[arg(long, value_name = "FACTOR", value_parser = factor)]
    pub timeout_multiplier: Option<f64>,

    /// Lower bound on the mutant timeout, however fast the baseline was.
    ///
    /// A suite that finishes in a second gets a budget of just over a second, which a loaded
    /// machine can miss for reasons that have nothing to do with the mutant.
    #[arg(long, value_name = "SECONDS", value_parser = seconds)]
    pub minimum_test_timeout: Option<f64>,

    /// How much memory control to place around each test binary. Off by default.
    ///
    /// A mutation can turn bounded allocation into unbounded allocation, which the timeout only
    /// stops after the machine has already been driven into swap. `measure` records what each test
    /// binary's whole process tree uses during the baseline and reports it, without ever stopping a
    /// mutant. `enforce` also holds each mutant to a ceiling derived from that measurement.
    ///
    /// Needs a delegated cgroup v2 on Linux, or a job object on Windows. Where neither is
    /// available the run says so and stops rather than pretend to be protected.
    #[arg(long, value_name = "MODE")]
    pub memory: Option<crate::exec::MemoryControl>,

    /// Multiple of a test binary's baseline peak memory a mutant of it may reach.
    #[arg(long, value_name = "FACTOR", value_parser = factor)]
    pub memory_multiplier: Option<f64>,

    /// Absolute headroom added to a test binary's baseline peak memory.
    ///
    /// The ceiling is the larger of this and the multiplier, so this is what governs a binary whose
    /// baseline peak is small enough that doubling it would still leave no room for a lazily
    /// initialized table or a randomized test that picked a larger input.
    #[arg(long, value_name = "SIZE", value_parser = size)]
    pub memory_headroom: Option<u64>,

    /// An explicit memory ceiling for every test binary, instead of one derived from the baseline.
    ///
    /// Implies `--memory enforce`, and is the only way to bound a run that skips the baseline,
    /// since there is then nothing to calibrate a ceiling from.
    #[arg(long, value_name = "SIZE", value_parser = size)]
    pub memory_limit: Option<u64>,

    /// A memory ceiling for the baseline runs themselves.
    ///
    /// A ceiling derived from the baseline cannot protect the machine from a baseline that is
    /// itself runaway, which is the risk the first time an unfamiliar suite is measured. Implies
    /// `--memory measure`.
    #[arg(long, value_name = "SIZE", value_parser = size)]
    pub baseline_memory_limit: Option<u64>,

    /// Which cargo profile to build with.
    ///
    /// Worth more here than in a per-mutant tool: the build is paid once and then thousands of
    /// mutants run against it, so an optimized profile usually pays for itself many times over.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Pass an argument through to every cargo invocation.
    #[arg(short = 'C', long = "cargo-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub cargo_args: Vec<String>,

    /// Pass an argument through to every test binary.
    #[arg(long = "cargo-test-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub cargo_test_args: Vec<String>,

    /// Only run the tests of these packages when deciding a verdict.
    ///
    /// Separate from `--package`, which chooses what to mutate. Narrowing this is usually the
    /// largest speedup available in a workspace whose crates are loosely coupled, because what a
    /// mutant costs is decided by how much of the suite it has to reach.
    #[arg(long = "test-package", value_name = "NAME")]
    pub test_packages: Vec<String>,

    /// Only let these test targets decide a verdict.
    ///
    /// Matches cargo target names — a package's unit tests take the name of the lib or bin they
    /// live in, and each file under `tests/` is a target named after the file. Globs use `*` and
    /// `?`. Finer than `--test-package`, which cannot separate a package's real tests from the
    /// conformance corpus sitting beside them.
    #[arg(long = "include-test", value_name = "GLOB")]
    pub include_tests: Vec<String>,

    /// Do not let these test targets decide a verdict.
    ///
    /// Applied after `--include-test`, so an exclusion always wins. The usual reason is a target
    /// that is slow, flaky, or not an oracle at all — a conformance or fuzz corpus whose failures
    /// say nothing about whether a mutant was noticed. A pattern matching no target is an error.
    #[arg(long = "exclude-test", value_name = "GLOB")]
    pub exclude_tests: Vec<String>,

    /// Run the whole workspace's tests for every mutant.
    ///
    /// The default is to run only the tests that can reach the mutated package.
    #[arg(long, conflicts_with = "test_packages")]
    pub test_workspace: bool,

    /// Put the scratch tree and its build artifacts here instead of under the workspace's `target`.
    ///
    /// Lets a read-only checkout be mutated, moves the copy off a slow or network filesystem, and
    /// gives concurrent runs somewhere separate to work. Build artifacts live here too, so reusing
    /// one directory across runs keeps them incremental while a fresh one starts cold.
    #[arg(long, value_name = "DIR")]
    pub scratch_dir: Option<Utf8PathBuf>,

    /// Arguments passed to every test binary, after `--`.
    ///
    /// The natural place to name the tests a run should consider, as in `-- --skip slow_`.
    #[arg(last = true, value_name = "TEST_ARGS")]
    pub test_args: Vec<String>,
}

/// Arguments for `run`.
#[derive(Debug, Args, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is an independent command-line flag, and grouping them would only obscure that"
)]
pub struct RunArgs {
    /// Which mutants to consider.
    #[command(flatten)]
    pub select: SelectArgs,

    /// How the build and the baseline are measured.
    #[command(flatten)]
    pub measure: MeasureArgs,

    /// How long the build may take.
    #[command(flatten)]
    pub limits: BuildLimitArgs,

    /// Fail the run if the mutation score is below this percentage.
    #[arg(long, value_name = "PERCENT", value_parser = percentage)]
    pub min_score: Option<f64>,

    /// Skip mutants that this earlier JSON report already resolved.
    ///
    /// Turns a long run into something incremental: only mutants that survived, or that were never
    /// reached, are tried again.
    #[arg(long, value_name = "REPORT")]
    pub iterate: Option<Utf8PathBuf>,

    /// List the mutants the suite caught, not just the ones it missed.
    ///
    /// A run reports what escaped, because that is what needs acting on. This shows the other side:
    /// what the suite actually killed, which is how you confirm it is testing what you think it is.
    #[arg(short = 'v', long)]
    pub caught: bool,

    /// List every mutant that could not be compiled, not just how many there were.
    ///
    /// A mutant that does not compile says nothing about the test suite, and a large workspace
    /// produces thousands of them, so the summary counts them instead. Ask for the list when the
    /// question is which constructs the encoding could not express.
    #[arg(short = 'V', long)]
    pub unviable: bool,

    /// Keep the scratch tree after the run and say where it is.
    #[arg(long)]
    pub leak_dirs: bool,

    /// Skip the baseline run.
    ///
    /// Faster, and strictly less trustworthy: without it there is no evidence that a failure was
    /// caused by the mutant rather than by the suite already being red.
    #[arg(long)]
    pub no_baseline: bool,

    /// Find and report mutants without building or running anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Write a self-contained HTML report to this path.
    ///
    /// The page embeds the viewer and the results, so it opens from a CI artifact, a file share
    /// or an air-gapped machine with no network access.
    #[arg(long, value_name = "PATH")]
    pub html: Option<Utf8PathBuf>,

    /// Write a `mutation-testing-elements` JSON report to this path.
    ///
    /// This is the interchange format the standard mutation report viewers consume, so it feeds
    /// the Azure DevOps and GitHub integrations without any translation.
    #[arg(long, value_name = "PATH")]
    pub json_report: Option<Utf8PathBuf>,

    /// Load the report viewer from a CDN instead of embedding it.
    ///
    /// Produces a much smaller file, at the cost of needing network access to read it.
    #[arg(long, requires = "html")]
    pub html_external: bool,

    /// Write a SARIF 2.1.0 log of the surviving mutants to this path.
    ///
    /// Uploading it with `github/codeql-action/upload-sarif` puts survivors in the security tab,
    /// where they can be tracked and dismissed per mutator rather than re-reported every night.
    #[arg(long, value_name = "PATH")]
    pub sarif: Option<Utf8PathBuf>,

    /// How loudly a survivor is reported to a SARIF consumer.
    ///
    /// A surviving mutant is an observation about the test suite rather than a defect in the code,
    /// and drowning the security tab is how a good signal gets turned off.
    #[arg(long, value_name = "LEVEL", default_value = "note")]
    pub sarif_level: Level,

    /// Annotate the diff and write a job summary when running inside a CI system.
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub annotations: Annotations,

    /// Write a Markdown diagnosis of where the time went, and what each mutator family bought.
    ///
    /// The analysis is a document rather than a verdict: it names what would make the run cheaper
    /// and, for every remedy, what that remedy costs in signal. It goes to a file rather than the
    /// console because it is written to be read later, shared, and pasted into a review.
    #[arg(long, value_name = "PATH")]
    pub advice: Option<Utf8PathBuf>,

    /// Project what the rest of the run will cost, once the build and baseline have been measured.
    ///
    /// Printed at the only moment it is both possible and useful: everything before it was
    /// measured, and everything after it is the wait you are deciding whether to sit through. The
    /// range assumes a killed mutant gets through 60% of the tests that can reach it before one
    /// of them fails.
    #[arg(long)]
    pub estimate: bool,

    /// Dump what the run measured about itself, for people working on this tool.
    ///
    /// Hidden, unstable and undocumented on purpose: it exists so that a change to the scheduler,
    /// the build sequencing or the mutator catalog can be judged against numbers instead of
    /// against how the run felt. Nothing here is a promise, and none of it is meant to be parsed.
    #[arg(long, hide = true)]
    pub diag: bool,

    /// Wait out the whole budget for every mutant instead of cutting off one that has stopped
    /// making progress.
    ///
    /// A hung mutant is normally detected as soon as its test binary goes quiet for longer than
    /// the baseline ever did, which is usually far sooner than its timeout. Turn this off if a
    /// test legitimately goes silent for much longer under mutation than it ever does healthy.
    #[arg(long)]
    pub no_stall_detection: bool,
}

/// Arguments for `completions`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// The shell to generate a completion script for.
    #[arg(value_name = "SHELL")]
    pub shell: Shell,
}

/// What `list` can enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListKind {
    /// The mutants that would be generated.
    Mutants,

    /// The mutator registry.
    Ops,

    /// The source files that would be analyzed.
    Files,
}

/// Arguments for `list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// What to list.
    #[arg(value_enum, default_value = "mutants")]
    pub what: ListKind,

    /// Which mutants to consider.
    #[command(flatten)]
    pub select: SelectArgs,

    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    pub json: bool,

    /// Write the population as a report document, for `merge` to withdraw retired mutants against.
    #[arg(long, value_name = "PATH")]
    pub json_report: Option<Utf8PathBuf>,
}

/// Arguments for `explain`.
#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// A mutator name, family, profile, or mutant id.
    #[arg(value_name = "SUBJECT")]
    pub subject: String,
}

impl Default for SelectArgs {
    fn default() -> Self {
        Self {
            dir: Utf8PathBuf::from("."),
            ops: None,
            files: Vec::new(),
            exclude_files: Vec::new(),
            shard_count: None,
            shard_index: None,
            in_diff: None,
            packages: Vec::new(),
            workspace: false,
            errors: Vec::new(),
            features: FeatureArgs::default(),
            config: ConfigArgs::default(),
        }
    }
}

impl SelectArgs {
    /// Resolves the `--ops` selector list into a concrete set of mutators.
    pub fn selection(&self) -> crate::Result<Selection> {
        let mut selection = self
            .ops
            .as_deref()
            .map_or_else(|| Ok(Selection::default_profile()), Selection::parse)?;

        // An explicit `--ops` list is the whole set, so `--error` must not smuggle a mutator into
        // it. Dropping the values silently would be worse still, so say so.
        if self.ops.is_some() && !self.errors.is_empty() && !selection.contains("fn_value.err_with") {
            selection.drop_errors();
            return Ok(selection);
        }

        selection.set_errors(self.errors.clone());
        Ok(selection)
    }

    /// Validates the sharding arguments.
    pub fn shard(&self) -> crate::Result<Option<(u32, u32)>> {
        match (self.shard_count, self.shard_index) {
            (Some(count), Some(index)) => {
                if count == 0 {
                    return Err(error!("--shard-count must be at least 1").usage());
                }

                if index >= count {
                    return Err(error!(
                        "--shard-index {index} is out of range for --shard-count {count}; valid indices are 0..{}",
                        count - 1
                    )
                    .usage());
                }

                Ok(Some((count, index)))
            }

            _ => Ok(None),
        }
    }
}

impl SelectArgs {
    /// Returns whether a package is one the run was asked to mutate.
    #[must_use]
    pub fn mutates_package(&self, name: &str) -> bool {
        self.packages.is_empty() || self.packages.iter().any(|wanted| wanted == name)
    }
}

impl FeatureArgs {
    /// Renders the selection as the cargo arguments that express it.
    #[must_use]
    pub fn to_cargo_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        if self.all_features {
            args.push("--all-features".to_owned());
        }

        if self.no_default_features {
            args.push("--no-default-features".to_owned());
        }

        if !self.features.is_empty() {
            args.push("--features".to_owned());
            args.push(self.features.join(","));
        }

        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_index_must_be_in_range() {
        let args = SelectArgs {
            shard_count: Some(4),
            shard_index: Some(4),
            ..SelectArgs::default()
        };

        let error = args.shard().unwrap_err().to_string();

        assert!(error.contains("out of range"), "{error}");
        assert!(error.contains("0..3"), "{error}");
    }

    #[test]
    fn a_zero_shard_count_is_rejected() {
        let args = SelectArgs {
            shard_count: Some(0),
            shard_index: Some(0),
            ..SelectArgs::default()
        };

        let _ = args.shard().unwrap_err();
    }

    #[test]
    fn a_valid_shard_is_accepted() {
        let args = SelectArgs {
            shard_count: Some(4),
            shard_index: Some(3),
            ..SelectArgs::default()
        };

        assert_eq!(args.shard().unwrap(), Some((4, 3)));
    }

    #[test]
    fn no_sharding_arguments_means_no_shard() {
        assert_eq!(SelectArgs::default().shard().unwrap(), None);
    }

    #[test]
    fn no_ops_argument_selects_the_default_profile() {
        let selection = SelectArgs::default().selection().unwrap();

        assert!(selection.contains("fn_value.default"));
        assert!(selection.contains("stmt.delete_call"));
    }

    #[test]
    fn naming_error_values_turns_the_error_mutator_on() {
        // The mutator is registered on like everything else, but it is inert until the user names
        // something for it to substitute, so supplying a value has to keep it on rather than being
        // the thing that enables it.
        let args = SelectArgs {
            errors: vec!["MyError::Io".to_owned()],
            ..SelectArgs::default()
        };

        let selection = args.selection().unwrap();

        assert!(selection.contains("fn_value.err_with"));
        assert_eq!(selection.errors(), ["MyError::Io".to_owned()]);
    }

    #[test]
    fn error_values_are_ignored_when_the_mutator_is_deselected() {
        let args = SelectArgs {
            ops: Some("relational".to_owned()),
            errors: vec!["MyError::Io".to_owned()],
            ..SelectArgs::default()
        };

        assert!(args.selection().unwrap().errors().is_empty());
    }

    #[test]
    fn a_package_filter_admits_only_what_it_names() {
        let args = SelectArgs {
            packages: vec!["alpha".to_owned()],
            ..SelectArgs::default()
        };

        assert!(args.mutates_package("alpha"));
        assert!(!args.mutates_package("beta"));
    }

    #[test]
    fn no_package_filter_admits_everything() {
        let args = SelectArgs::default();

        assert!(args.mutates_package("alpha"));
        assert!(args.mutates_package("beta"));
    }

    #[test]
    fn feature_arguments_render_as_cargo_spells_them() {
        let features = FeatureArgs {
            features: vec!["a,b".to_owned()],
            all_features: false,
            no_default_features: true,
        };

        assert_eq!(
            features.to_cargo_args(),
            vec![
                "--no-default-features".to_owned(),
                "--features".to_owned(),
                "a,b".to_owned()
            ]
        );
    }

    #[test]
    fn no_feature_arguments_render_as_nothing() {
        assert!(FeatureArgs::default().to_cargo_args().is_empty());
    }

    #[test]
    fn all_features_render_before_named_features() {
        let features = FeatureArgs {
            features: vec!["serde".to_owned(), "cli".to_owned()],
            all_features: true,
            no_default_features: false,
        };

        assert_eq!(
            features.to_cargo_args(),
            vec![
                "--all-features".to_owned(),
                "--features".to_owned(),
                "serde,cli".to_owned()
            ]
        );
    }

    #[test]
    fn the_cli_definition_is_valid() {
        use clap::CommandFactory as _;

        Cli::command().debug_assert();
    }
}
