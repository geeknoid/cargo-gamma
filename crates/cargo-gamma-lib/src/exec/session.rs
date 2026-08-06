use core::time::Duration;

use super::test_binary::TestBinary;

/// What happened during a run, beyond the verdicts written back onto the mutants.
#[derive(Debug, Clone)]
pub struct Session {
    /// How long the baseline suite took.
    pub baseline: Duration,

    /// The longest the baseline legitimately went without saying anything.
    pub quiet: Duration,

    /// The silence a mutant was allowed before it was presumed hung, when that was enabled.
    pub stall: Option<Duration>,

    /// The budget each mutant was given, across every binary it has to run.
    pub timeout: Duration,

    /// How long the single build took.
    pub build: Duration,

    /// The largest peak memory any one test binary reached during the baseline.
    ///
    /// `None` when nothing measured it, which is both the default and what a host without an
    /// aggregate process-tree accounting facility can offer. Reported because it is the figure a
    /// memory ceiling is chosen from, and because a suite whose peak surprises its authors is worth
    /// knowing about whether or not a ceiling is being enforced.
    pub peak: Option<u64>,

    /// Whether this run actually metered memory, which is not always what was configured.
    ///
    /// Memory control is on by default, and a host without cgroup v2 delegation cannot provide it.
    /// A run that defaulted into it and could not have it degrades rather than stopping, so the
    /// configured policy is a request and this is the answer. Everything downstream reads this one,
    /// because asking the platform for accounting it already declined to give would fail every
    /// mutant in the sweep.
    pub metered: bool,

    /// Why memory went unbounded, when it was meant to be bounded and could not be.
    ///
    /// Carried to the end of the run rather than printed when it is discovered, because progress
    /// output is suppressed when nothing is watching it — and a CI runner with no cgroup delegation
    /// is exactly the case where the protection is missing *and* nobody sees the transient line
    /// saying so.
    pub unbounded: Option<String>,

    /// How many mutants were withdrawn because they could not compile.
    pub withdrawn: usize,

    /// How many rollback rounds were needed.
    pub rounds: u32,

    /// The test binaries that were run.
    pub binaries: Vec<TestBinary>,

    /// How many bytes the scratch directory holds once the run has built everything.
    ///
    /// Reported because it is a real operating cost rather than a curiosity: a large workspace can
    /// leave tens of gigabytes here, which is more than the free space on a common CI runner, and a
    /// job whose next step fails for want of disk deserves to know where the disk went.
    pub footprint: u64,

    /// How many test targets `--include-test` or `--exclude-test` kept out of the oracle.
    ///
    /// Zero unless one of those was given. Reported because a narrowed oracle is the single most
    /// consequential thing that can happen to a score without appearing anywhere in it: a survivor
    /// here may be a mutant the excluded target would have caught, and a reader who did not write
    /// the `gamma.toml` has no other way to know the suite was not asked in full.
    pub filtered: usize,

    /// How many mutants sit in source the build never compiled.
    ///
    /// Reported because it is the difference between the population `gamma list` names and the one
    /// this run judged, and because the fix is a feature flag rather than anything in the code.
    pub not_built: usize,

    /// Whether the run had to build test targets it knew it would never consult.
    ///
    /// Building only the packages whose tests can reach a mutant is the cheaper thing to do, but
    /// cargo unifies features over the packages it is asked to build, so a test target that only
    /// compiles because some other package switches a feature on will not compile on its own. When
    /// that happens the selection is abandoned and the whole workspace is built, and the run says
    /// so: the scope the user asked for did not survive contact with their feature graph.
    pub widened: bool,
}
