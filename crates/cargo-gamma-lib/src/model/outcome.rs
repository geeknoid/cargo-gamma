use core::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

/// The verdict for a single mutant.
///
/// The names follow the `mutation-testing-elements` schema so that our reports and the standard
/// report UI agree on vocabulary without a translation layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    /// Not yet run.
    Pending,

    /// A test failed while this mutant was active: the test suite detected the change.
    Killed,

    /// Every test passed while this mutant was active: the change went unnoticed.
    Survived,

    /// The test run exceeded its budget. Counted as detected, because a hang is a behavior change
    /// the suite noticed, even though it noticed it expensively.
    Timeout,

    /// The mutant's test run passed the memory ceiling derived from that binary's own baseline.
    ///
    /// Counted as detected, on the same reasoning as `Timeout`: the baseline established that this
    /// workload fits under this ceiling without the mutant, so the mutant is what changed. It is a
    /// variant of its own rather than a flavour of `Killed` because the suite's assertions did not
    /// fail — the kernel stopped the workload — and a reader who cannot tell those apart will go
    /// looking for a failing test that does not exist.
    OutOfMemory,

    /// The mutant could not be compiled. Not a test-suite failing.
    CompileError,

    /// Suppressed by a directive, an attribute, or configuration.
    Ignored,

    /// No test reaches this mutant's site.
    NoCoverage,

    /// The build never compiled this mutant's source file, so it was never a candidate.
    ///
    /// Conditional compilation is the reason: a module behind `#[cfg(feature = "serde")]` is real
    /// source that a run without that feature never builds. Mutants are found by reading files, so
    /// they are generated there anyway, and the instrumented tree compiles perfectly well because
    /// the code holding them is not part of it.
    ///
    /// Such a mutant used to be reported as a survivor, which is the worst available answer: no
    /// test can fail for code that was never built, so it looked like a test-suite gap that nobody
    /// could ever close. On a crate whose `serde` support is not a default feature this was most of
    /// the survivor list and moved the score by thirty points.
    ///
    /// Excluded from the score for the same reason `CompileError` is — a mutant that never ran is
    /// not evidence about the tests — and named separately so the run can say the feature set is
    /// the reason and the reader can decide whether to widen it.
    NotBuilt,
}

impl Outcome {
    /// Returns whether this outcome counts as the suite having detected the mutant.
    #[must_use]
    pub const fn is_detected(self) -> bool {
        matches!(self, Self::Killed | Self::Timeout | Self::OutOfMemory)
    }

    /// Returns whether this outcome contributes to the mutation score at all.
    ///
    /// Compile errors, suppressions and mutants the build never compiled are excluded from the
    /// denominator: a mutant that never ran is not evidence about the test suite, and counting it
    /// either way would be a lie.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        matches!(self, Self::Killed | Self::Survived | Self::Timeout | Self::OutOfMemory | Self::NoCoverage)
    }

    /// Returns the short lowercase name used in output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Killed => "killed",
            Self::Survived => "survived",
            Self::Timeout => "timeout",
            Self::OutOfMemory => "outofmem",
            Self::CompileError => "unviable",
            Self::Ignored => "ignored",
            Self::NoCoverage => "uncovered",
            Self::NotBuilt => "notbuilt",
        }
    }
}

impl Display for Outcome {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_classification() {
        assert!(Outcome::Killed.is_detected());
        assert!(Outcome::Timeout.is_detected());
        assert!(Outcome::OutOfMemory.is_detected());
        assert!(!Outcome::Survived.is_detected());

        assert!(Outcome::OutOfMemory.is_valid());
        assert!(Outcome::Survived.is_valid());
        assert!(Outcome::NoCoverage.is_valid());
        assert!(!Outcome::CompileError.is_valid());
        assert!(!Outcome::Ignored.is_valid());
        assert!(!Outcome::Pending.is_valid());
    }

    #[test]
    fn every_outcome_has_the_short_name_used_in_reports() {
        // These strings are user-facing and serialized in text reports, so changing one is a
        // compatibility break rather than a cosmetic edit.
        assert_eq!(Outcome::Pending.as_str(), "pending");
        assert_eq!(Outcome::Killed.as_str(), "killed");
        assert_eq!(Outcome::Survived.as_str(), "survived");
        assert_eq!(Outcome::Timeout.as_str(), "timeout");
        assert_eq!(Outcome::OutOfMemory.as_str(), "outofmem");
        assert_eq!(Outcome::CompileError.as_str(), "unviable");
        assert_eq!(Outcome::Ignored.as_str(), "ignored");
        assert_eq!(Outcome::NoCoverage.as_str(), "uncovered");
    }
}
