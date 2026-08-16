//! How an incremental run reuses state from the previous run.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// How an incremental run reuses state from the previous run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IncrementalMode {
    /// Re-run everything from scratch with no caching.
    #[value(alias = "none", alias = "off")]
    No,

    /// Reuse only compiler unviability results for unchanged files.
    Build,

    /// Reuse compiler unviability and test verdicts for unchanged files and tests.
    #[default]
    #[value(alias = "all")]
    Full,
}

impl IncrementalMode {
    /// Whether any caching or reuse is enabled.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::No)
    }

    /// Whether test verdicts are reused.
    #[must_use]
    pub const fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_full` gates whether prior-run test verdicts are reused. `Build` reuses only compiler
    /// unviability, so it must not read as full, or an `--incremental build` run would trust stale
    /// verdicts it was explicitly told to discard.
    #[test]
    fn only_full_reuses_test_verdicts() {
        assert!(IncrementalMode::Full.is_full());
        assert!(!IncrementalMode::Build.is_full());
        assert!(!IncrementalMode::No.is_full());
    }
}
