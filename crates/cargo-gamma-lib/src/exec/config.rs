use camino::Utf8PathBuf;
use core::num::NonZero;
use core::time::Duration;
use std::thread;

use super::cargo_options::{BuildLimits, CargoOptions};
use super::memory::MemoryPolicy;

/// Knobs for a run.
#[expect(
    clippy::struct_excessive_bools,
    reason = "an options bag mirroring independent command-line flags, not a state machine"
)]
#[derive(Debug, Clone)]
pub struct Config {
    /// How many mutants to test at once.
    pub jobs: usize,

    /// Multiple of the baseline duration a mutant is allowed before it is called a timeout.
    pub timeout_multiplier: f64,

    /// Lower bound on the timeout, so that a suite which finishes instantly does not produce a
    /// budget so tight that scheduler noise reads as a hang.
    pub timeout_floor: Duration,

    /// An explicit timeout, overriding the baseline-derived one.
    pub timeout: Option<Duration>,

    /// Whether to run the baseline. Skipping it is faster and strictly less trustworthy.
    pub baseline: bool,

    /// Whether to cut a mutant off as soon as its test binary stops reporting progress.
    pub stall: bool,

    /// Multiple of the longest silence the baseline produced that a mutant is allowed.
    pub stall_factor: f64,

    /// Lower bound on the stall budget, so a suite that never goes quiet does not produce a budget
    /// that scheduler noise can trip.
    pub stall_floor: Duration,

    /// How cargo and the test binaries are invoked.
    pub cargo: CargoOptions,

    /// How long the build may take.
    pub build: BuildLimits,

    /// How much memory a mutant may use, and whether anything enforces it.
    pub memory: MemoryPolicy,

    /// Keep the scratch tree after the run instead of deleting it.
    pub leak_dirs: bool,

    /// Where to put the scratch tree and its build artifacts. `None` puts them under the
    /// workspace's own `target` directory.
    pub scratch_dir: Option<Utf8PathBuf>,

    /// Packages whose tests decide a verdict. Empty means whichever can reach the mutant.
    pub test_packages: Vec<String>,

    /// Test target name globs whose tests may decide a verdict. Empty means all of them.
    pub include_tests: Vec<String>,

    /// Test target name globs whose tests must not decide a verdict.
    pub exclude_tests: Vec<String>,

    /// Run every test in the workspace for every mutant, rather than only those that can reach it.
    pub test_workspace: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jobs: thread::available_parallelism().map_or(1, NonZero::get),
            timeout_multiplier: 1.2,
            timeout_floor: Duration::from_secs(20),
            timeout: None,
            baseline: true,
            stall: true,
            stall_factor: 10.0,
            stall_floor: Duration::from_secs(5),
            cargo: CargoOptions::default(),
            build: BuildLimits::default(),
            memory: MemoryPolicy::default(),
            leak_dirs: false,
            scratch_dir: None,
            test_packages: Vec::new(),
            include_tests: Vec::new(),
            exclude_tests: Vec::new(),
            test_workspace: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_timeout_never_falls_below_the_floor() {
        let config = Config::default();
        let scaled = Duration::from_millis(1).mul_f64(config.timeout_multiplier);

        assert_eq!(scaled.max(config.timeout_floor), config.timeout_floor);
    }

    #[test]
    fn the_default_timeout_scales_with_a_slow_baseline() {
        let config = Config::default();
        let baseline = Duration::from_secs(60);
        let scaled = baseline.mul_f64(config.timeout_multiplier);

        assert_eq!(scaled.max(config.timeout_floor), Duration::from_secs(72));
    }

    #[test]
    fn the_default_job_count_is_at_least_one() {
        assert!(Config::default().jobs >= 1);
    }

    /// Memory control is off until it is asked for.
    #[test]
    fn memory_control_is_enforced_by_default() {
        // On the same footing as the wall-clock timeout: a mutation can turn bounded allocation
        // into unbounded allocation, and the user who most needs protecting from that is the one
        // who never thought to ask. The ceiling is derived from each binary's own baseline peak, so
        // it is a statement about this suite rather than a guess about suites in general.
        let config = Config::default();

        assert!(config.memory.measuring());
        assert!(config.memory.enforcing());
        assert!(config.memory.ceiling(Some(1024 * 1024), true).is_some());

        // Nobody asked for it, so a host that cannot deliver it degrades rather than refusing.
        assert!(!config.memory.insisted());
    }
}
