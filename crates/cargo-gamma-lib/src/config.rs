//! The project configuration file, `.cargo/gamma.toml`.
//!
//! A mutation run has a lot of knobs, and a project that has settled on a set of them should not
//! have to repeat that set in every CI job and every developer's shell history. Anything expressible
//! on the command line is expressible here.
//!
//! Two decisions shape the whole module.
//!
//! **Unknown keys are errors.** A configuration file whose settings are silently ignored is worse
//! than no configuration file, because the project believes it is configured. A misspelled key, or a
//! key for a feature this build does not have, stops the run and names the offender.
//!
//! **`.cargo/mutants.toml` is never read.** It is a different schema for a different tool, and
//! honouring it silently would mean another tool's `exclude_re` entries quietly changing which
//! mutants this one suppresses. `cargo gamma migrate` translates it explicitly, once.

use std::fs;
use std::io::ErrorKind;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use crate::bounds;
use crate::Result;
use crate::commands::{RunArgs, SelectArgs};
use crate::error::error;

/// Where the file lives, relative to the directory being analyzed.
const RELATIVE_PATH: &str = ".cargo/gamma.toml";

/// The name of the file cargo-mutants uses, which is deliberately not read.
const FOREIGN_PATH: &str = ".cargo/mutants.toml";

/// A parsed `.cargo/gamma.toml`.
///
/// Every field is optional: a file that sets one key is a valid file, and the rest keep whatever
/// the command line or the built-in default says.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// The mutator selector list, as it would be written after `--ops`.
    ///
    /// A list rather than a string, because a configuration file has room to put one selector per
    /// line with a comment explaining why. The entries are joined with commas and parsed by exactly
    /// the same code that parses the flag, so the two cannot drift.
    pub ops: Option<Vec<String>>,

    /// Globs limiting which files are mutated.
    pub files: Vec<String>,

    /// Globs excluding files from mutation.
    pub exclude_files: Vec<String>,

    /// Fail the run below this mutation score.
    pub min_score: Option<f64>,

    /// How many mutants to test at once.
    pub jobs: Option<usize>,

    /// A fixed per-mutant timeout, in seconds.
    pub timeout: Option<f64>,

    /// The multiple of the baseline duration a mutant is allowed.
    pub timeout_multiplier: Option<f64>,

    /// Skip the baseline measurement.
    pub no_baseline: Option<bool>,

    /// Packages to mutate. Empty means every package in the workspace.
    pub packages: Vec<String>,

    /// Packages whose tests decide a verdict. Empty means whichever can reach the mutant.
    pub test_packages: Vec<String>,

    /// Test target name globs whose tests may decide a verdict. Empty means all of them.
    pub include_tests: Vec<String>,

    /// Test target name globs whose tests must not decide a verdict.
    pub exclude_tests: Vec<String>,

    /// Cargo features to activate.
    pub features: Vec<String>,

    /// Activate every feature of every selected package.
    pub all_features: Option<bool>,

    /// Do not activate the `default` feature.
    pub no_default_features: Option<bool>,

    /// The cargo profile to build with.
    pub profile: Option<String>,

    /// Extra arguments for every cargo invocation.
    pub cargo_args: Vec<String>,

    /// Extra arguments for every test binary.
    pub cargo_test_args: Vec<String>,

    /// Additional `Err(...)` values for `fn_value.err_with`.
    pub errors: Vec<String>,

    /// A lower bound on the per-mutant timeout, in seconds.
    pub minimum_test_timeout: Option<f64>,

    /// How much memory control to place around each test binary.
    pub memory: Option<crate::exec::MemoryControl>,

    /// The multiple of a test binary's baseline peak memory a mutant of it may reach.
    pub memory_multiplier: Option<f64>,

    /// Absolute headroom added to a test binary's baseline peak memory, as a size such as `128MiB`.
    pub memory_headroom: Option<String>,

    /// An explicit memory ceiling for every test binary, as a size such as `2GiB`.
    pub memory_limit: Option<String>,

    /// A memory ceiling for the baseline runs themselves, as a size such as `4GiB`.
    pub baseline_memory_limit: Option<String>,

    /// A fixed build timeout, in seconds.
    pub build_timeout: Option<f64>,

    /// The multiple of the first build's duration a later build round is allowed.
    pub build_timeout_multiplier: Option<f64>,

    /// Sharding.
    #[serde(default)]
    pub shard: Shard,

    /// File reports.
    #[serde(default)]
    pub reporters: Reporters,
}

/// Reads a size key that [`Config::validate`] has already accepted.
///
/// A key that did not parse stopped the run before this point, so there is nothing left here to
/// report and nothing to fall back to but leaving the setting unset.
fn size(text: Option<&str>) -> Option<u64> {
    let text = text?;

    bounds::size(text).ok()
}

/// The `[shard]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Shard {
    /// How many shards to divide the mutants into.
    pub count: Option<u32>,

    /// Which shard to run, from zero.
    pub index: Option<u32>,
}

/// The `[reporters]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Reporters {
    /// Where to write the self-contained HTML report.
    pub html: Option<Utf8PathBuf>,

    /// Where to write the `mutation-testing-elements` JSON report.
    pub json: Option<Utf8PathBuf>,

    /// Load the viewer from a CDN instead of embedding it.
    pub html_external: Option<bool>,

    /// Where to write the SARIF log of surviving mutants.
    pub sarif: Option<Utf8PathBuf>,
}

impl Config {
    /// Loads the configuration named by the command line, honouring `--config` and `--no-config`.
    ///
    /// An explicit path must exist: asking for a file and silently getting the defaults because it
    /// was misspelled is the failure this guards against, whereas a missing conventional file is
    /// the ordinary case.
    pub fn resolve(select: &SelectArgs) -> Result<Self> {
        if select.config.no_config {
            return Ok(Self::default());
        }

        let Some(path) = select.config.path.as_ref() else {
            return Self::load(&select.dir);
        };

        let text = fs::read_to_string(path).map_err(|cause| error!("could not read {path}").caused_by(cause))?;

        Self::parse(&text).map_err(|cause| error!("{path}: {cause}").usage())
    }

    /// Loads the configuration for a directory, if there is one.
    ///
    /// Returns the default configuration when the file is absent, which is the overwhelmingly
    /// common case and is not worth distinguishing from an empty file.
    pub fn load(dir: &Utf8Path) -> Result<Self> {
        let path = dir.join(RELATIVE_PATH);

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,

            Err(cause) if cause.kind() == ErrorKind::NotFound => return Ok(Self::default()),

            Err(cause) => return Err(error!("could not read {path}").caused_by(cause)),
        };

        Self::parse(&text).map_err(|cause| error!("{path}: {cause}").usage())
    }

    /// Parses configuration text.
    ///
    /// Separated from [`Self::load`] so the schema can be tested without touching a file system.
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|cause| {
            // toml's own message carries the line, the column and a caret, so it is a better
            // diagnostic than anything reconstructed here would be.
            cause.message().to_owned()
        })?;

        config.validate()?;

        Ok(config)
    }

    /// Range-checks the numeric keys.
    ///
    /// The command line checks the same values through its own parsers, but a setting can arrive
    /// from either place and only one of the two would otherwise be guarded.
    fn validate(&self) -> Result<(), String> {
        /// A key, its value if set, and the range check that applies to it.
        type Check = (&'static str, Option<f64>, fn(&str, f64) -> Result<f64, String>);

        let checks: [Check; 7] = [
            ("timeout", self.timeout, bounds::seconds),
            ("timeout-multiplier", self.timeout_multiplier, bounds::factor),
            ("minimum-test-timeout", self.minimum_test_timeout, bounds::seconds),
            ("build-timeout", self.build_timeout, bounds::seconds),
            ("build-timeout-multiplier", self.build_timeout_multiplier, bounds::factor),
            ("min-score", self.min_score, bounds::percentage),
            ("memory-multiplier", self.memory_multiplier, bounds::factor),
        ];

        for (key, value, check) in checks {
            if let Some(value) = value {
                let _checked = check(&value.to_string(), value).map_err(|cause| format!("{key}: {cause}"))?;
            }
        }

        let sizes = [
            ("memory-headroom", self.memory_headroom.as_deref()),
            ("memory-limit", self.memory_limit.as_deref()),
            ("baseline-memory-limit", self.baseline_memory_limit.as_deref()),
        ];

        for (key, value) in sizes {
            if let Some(value) = value {
                let _checked = bounds::size(value).map_err(|cause| format!("{key}: {cause}"))?;
            }
        }

        Ok(())
    }

    /// Reports whether a cargo-mutants configuration file exists but is not being read.
    ///
    /// A project that migrates by copying its old file into place and changing nothing would
    /// otherwise see its settings silently do nothing at all, so the run says so out loud.
    #[must_use]
    pub fn foreign_present(dir: &Utf8Path) -> bool {
        dir.join(FOREIGN_PATH).is_file() && !dir.join(RELATIVE_PATH).is_file()
    }

    /// Applies the configuration underneath the command line.
    ///
    /// Scalars set on the command line win outright: a flag typed for this one run is the most
    /// specific statement of intent available. Lists concatenate, with the command line first, so a
    /// configured exclusion cannot be lost by adding one more on the command line.
    pub fn apply(&self, args: &mut RunArgs) {
        self.apply_selection(&mut args.select);

        args.min_score = args.min_score.or(self.min_score);
        args.measure.jobs = args.measure.jobs.or(self.jobs);
        args.measure.timeout = args.measure.timeout.or(self.timeout);
        args.measure.timeout_multiplier = args.measure.timeout_multiplier.or(self.timeout_multiplier);
        args.measure.minimum_test_timeout = args.measure.minimum_test_timeout.or(self.minimum_test_timeout);
        args.measure.memory = args.measure.memory.or(self.memory);
        args.measure.memory_multiplier = args.measure.memory_multiplier.or(self.memory_multiplier);
        args.measure.memory_headroom = args.measure.memory_headroom.or_else(|| size(self.memory_headroom.as_deref()));
        args.measure.memory_limit = args.measure.memory_limit.or_else(|| size(self.memory_limit.as_deref()));
        args.measure.baseline_memory_limit = args
            .measure
            .baseline_memory_limit
            .or_else(|| size(self.baseline_memory_limit.as_deref()));
        args.limits.build_timeout = args.limits.build_timeout.or(self.build_timeout);
        args.limits.build_timeout_multiplier = args.limits.build_timeout_multiplier.or(self.build_timeout_multiplier);
        args.measure.profile = args.measure.profile.take().or_else(|| self.profile.clone());
        args.measure.cargo_args.extend(self.cargo_args.iter().cloned());
        args.measure.cargo_test_args.extend(self.cargo_test_args.iter().cloned());
        args.measure.test_packages.extend(self.test_packages.iter().cloned());
        args.measure.include_tests.extend(self.include_tests.iter().cloned());
        args.measure.exclude_tests.extend(self.exclude_tests.iter().cloned());
        args.no_baseline = args.no_baseline || self.no_baseline.unwrap_or(false);
        args.html = args.html.take().or_else(|| self.reporters.html.clone());
        args.json_report = args.json_report.take().or_else(|| self.reporters.json.clone());
        args.sarif = args.sarif.take().or_else(|| self.reporters.sarif.clone());
        args.html_external = args.html_external || self.reporters.html_external.unwrap_or(false);
    }

    /// Applies the selection keys, which `list` and `explain` also need.
    pub fn apply_selection(&self, select: &mut SelectArgs) {
        if select.ops.is_none()
            && let Some(ops) = self.ops.as_ref()
        {
            select.ops = Some(ops.join(","));
        }

        select.files.extend(self.files.iter().cloned());
        select.exclude_files.extend(self.exclude_files.iter().cloned());
        select.packages.extend(self.packages.iter().cloned());
        select.errors.extend(self.errors.iter().cloned());
        select.features.features.extend(self.features.iter().cloned());
        select.features.all_features = select.features.all_features || self.all_features.unwrap_or(false);
        select.features.no_default_features =
            select.features.no_default_features || self.no_default_features.unwrap_or(false);
        select.shard_count = select.shard_count.or(self.shard.count);
        select.shard_index = select.shard_index.or(self.shard.index);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn select_args(dir: &Utf8Path) -> SelectArgs {
        SelectArgs {
            dir: dir.to_path_buf(),
            ..SelectArgs::default()
        }
    }

    #[test]
    fn no_config_wins_over_a_file_that_is_there() {
        let dir = TempDir::new().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 path");

        fs::create_dir_all(root.join(".cargo")).expect("create");
        fs::write(root.join(RELATIVE_PATH), "jobs = 7\n").expect("write");

        let mut select = select_args(root);

        select.config.no_config = true;
        assert_eq!(Config::resolve(&select).expect("resolves").jobs, None);

        select.config.no_config = false;
        assert_eq!(Config::resolve(&select).expect("resolves").jobs, Some(7));
    }

    #[test]
    fn an_explicit_config_path_is_read_instead_of_the_default_one() {
        let dir = TempDir::new().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 path");
        let elsewhere = root.join("elsewhere.toml");

        fs::create_dir_all(root.join(".cargo")).expect("create");
        fs::write(root.join(RELATIVE_PATH), "jobs = 7\n").expect("write");
        fs::write(&elsewhere, "jobs = 3\n").expect("write");

        let mut select = select_args(root);

        select.config.path = Some(elsewhere);
        assert_eq!(Config::resolve(&select).expect("resolves").jobs, Some(3));
    }

    #[test]
    fn an_explicit_config_path_that_is_missing_is_an_error() {
        // An absent default file is ordinary; an absent file the user named by hand is a typo, and
        // silently running with no configuration would hide it.
        let dir = TempDir::new().expect("temp dir");
        let root = Utf8Path::from_path(dir.path()).expect("utf-8 path");
        let mut select = select_args(root);

        select.config.path = Some(root.join("nope.toml"));
        let _cause = Config::resolve(&select).unwrap_err();
    }

    #[test]
    fn memory_sizes_in_the_file_are_parsed_and_merged_into_the_arguments() {
        let config = Config::parse(
            "memory = \"enforce\"\nmemory-headroom = \"256MiB\"\nmemory-limit = \"2GiB\"\nbaseline-memory-limit = \"4GiB\"\n",
        )
        .expect("parses");

        let mut args = RunArgs::default();

        config.apply(&mut args);

        assert_eq!(args.measure.memory, Some(crate::exec::MemoryControl::Enforce));
        assert_eq!(args.measure.memory_headroom, Some(256 * 1024 * 1024));
        assert_eq!(args.measure.memory_limit, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(args.measure.baseline_memory_limit, Some(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn a_memory_size_that_is_not_a_size_is_reported_rather_than_ignored() {
        // A ceiling read as a handful of bytes would report every mutant as caught by tests that
        // could never have started, which is a far more expensive failure than a rejected file.
        let cause = Config::parse("memory-limit = \"lots\"\n").expect_err("must be rejected");

        assert!(cause.contains("memory-limit"), "{cause}");
    }

    #[test]
    fn an_empty_file_is_valid() {
        let config = Config::parse("").expect("an empty file is a valid file");

        assert!(config.ops.is_none());
        assert!(config.files.is_empty());
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_a_silent_no_op() {
        // The whole point of `deny_unknown_fields`: a project that believes it has configured
        // something and has not is in a worse position than one with no configuration at all.
        let cause = Config::parse("exclude-file = [\"src/main.rs\"]\n").expect_err("must be rejected");

        assert!(cause.contains("unknown field"), "{cause}");
    }

    #[test]
    fn a_misspelled_key_in_a_table_is_also_an_error() {
        let cause = Config::parse("[shard]\ncount = 4\nidx = 0\n").expect_err("must be rejected");

        assert!(cause.contains("unknown field"), "{cause}");
    }

    #[test]
    fn keys_are_spelled_in_kebab_case() {
        let config = Config::parse("exclude-files = [\"tests/**\"]\ntimeout-multiplier = 3.0\n")
            .expect("kebab-case is the file's spelling");

        assert_eq!(config.exclude_files, vec!["tests/**".to_owned()]);
        assert_eq!(config.timeout_multiplier, Some(3.0));
    }

    #[test]
    fn ops_are_joined_into_the_selector_list_the_flag_parses() {
        // One selector per line, with room for a comment, is the reason this is a list. It has to
        // arrive at exactly the same parser the flag uses, or the two spellings will drift.
        let config = Config::parse("ops = [\"@arithmetic\", \"!bitwise\"]\n").expect("parses");
        let mut select = SelectArgs::default();

        config.apply_selection(&mut select);

        assert_eq!(select.ops.as_deref(), Some("@arithmetic,!bitwise"));
    }

    #[test]
    fn the_command_line_wins_for_scalars() {
        let config = Config::parse("ops = [\"stmt\"]\nmin-score = 10.0\njobs = 1\n").expect("parses");
        let mut args = RunArgs {
            select: SelectArgs {
                ops: Some("relational".to_owned()),
                ..SelectArgs::default()
            },
            min_score: Some(90.0),
            ..RunArgs::default()
        };

        config.apply(&mut args);

        assert_eq!(args.select.ops.as_deref(), Some("relational"));
        assert_eq!(args.min_score, Some(90.0));

        // A key the command line did not speak to still applies.
        assert_eq!(args.measure.jobs, Some(1));
    }

    #[test]
    fn lists_concatenate_rather_than_replace() {
        // Replacing would mean that adding one exclusion on the command line silently drops every
        // exclusion the project has agreed on, which is the opposite of what typing it means.
        let config = Config::parse("exclude-files = [\"generated/**\"]\n").expect("parses");
        let mut args = RunArgs {
            select: SelectArgs {
                exclude_files: vec!["tests/**".to_owned()],
                ..SelectArgs::default()
            },
            ..RunArgs::default()
        };

        config.apply(&mut args);

        assert_eq!(args.select.exclude_files, vec!["tests/**".to_owned(), "generated/**".to_owned()]);
    }

    #[test]
    fn a_configured_flag_turns_on_and_the_command_line_cannot_turn_it_off() {
        // Boolean flags have no "off" spelling on the command line, so the configured value can
        // only ever add. This is worth a test because it is the one place the precedence rule
        // above does not apply, and it is easy to "fix" into a bug.
        let config = Config::parse("no-baseline = true\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args);

        assert!(args.no_baseline);
    }

    #[test]
    fn reporter_paths_come_from_the_file_when_the_command_line_is_silent() {
        let config = Config::parse(
            "[reporters]\nhtml = \"out/report.html\"\njson = \"out/report.json\"\nsarif = \"out/report.sarif\"\n",
        )
            .expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args);

        assert_eq!(args.html.as_deref(), Some(Utf8Path::new("out/report.html")));
        assert_eq!(args.json_report.as_deref(), Some(Utf8Path::new("out/report.json")));
        assert_eq!(args.sarif.as_deref(), Some(Utf8Path::new("out/report.sarif")));
    }

    #[test]
    fn sharding_can_be_set_entirely_from_the_file() {
        let config = Config::parse("[shard]\ncount = 30\nindex = 7\n").expect("parses");
        let mut args = RunArgs::default();

        config.apply(&mut args);

        assert_eq!(args.select.shard().expect("valid sharding"), Some((30, 7)));
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        let config = Config::load(path).expect("an absent file is the common case");

        assert!(config.ops.is_none());
    }

    #[test]
    fn a_present_file_is_read() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::create_dir_all(path.join(".cargo")).expect("could not create .cargo");
        fs::write(path.join(RELATIVE_PATH), "jobs = 3\n").expect("could not write the config");

        let config = Config::load(path).expect("the file is valid");

        assert_eq!(config.jobs, Some(3));
    }

    #[test]
    fn a_malformed_file_is_a_usage_error_naming_the_path() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::create_dir_all(path.join(".cargo")).expect("could not create .cargo");
        fs::write(path.join(RELATIVE_PATH), "jobs = \n").expect("could not write the config");

        let cause = Config::load(path).expect_err("a malformed file must stop the run");

        assert!(cause.is_usage(), "{cause}");
        assert!(cause.to_string().contains("gamma.toml"), "{cause}");
    }

    #[test]
    fn a_cargo_mutants_file_is_noticed_but_never_read() {
        // Reading it would mean another tool's settings quietly changing which mutants are
        // suppressed here. Noticing it is what lets the run say so out loud.
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::create_dir_all(path.join(".cargo")).expect("could not create .cargo");
        fs::write(path.join(FOREIGN_PATH), "exclude_re = [\"impl Debug\"]\n")
            .expect("could not write the foreign config");

        assert!(Config::foreign_present(path));

        let config = Config::load(path).expect("the foreign file must not be parsed as ours");

        assert!(config.ops.is_none());
    }

    #[test]
    fn a_config_that_cannot_be_read_is_an_error_rather_than_the_defaults() {
        // Only an absent file means "this project has no configuration". Anything else — a
        // directory in its place, a permission problem — has to be reported, because silently
        // falling back to the defaults would run with settings nobody chose.
        let dir = TempDir::new().expect("a temporary directory");
        let path = Utf8Path::from_path(dir.path()).expect("path is not UTF-8");

        fs::create_dir_all(path.join(RELATIVE_PATH)).expect("could not create a directory in the config's place");

        let error = Config::load(path).expect_err("an unreadable config must not be treated as absent");

        assert!(error.to_string().contains(RELATIVE_PATH), "{error}");
    }
}
