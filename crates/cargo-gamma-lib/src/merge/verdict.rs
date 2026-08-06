//! One mutant's most recent verdict, and when it was earned.

use crate::elements::MutantResult;

/// One mutant's most recent verdict, and when it was earned.
#[derive(Debug, Clone)]
pub(super) struct Verdict {
    /// The mutant as reported.
    pub(super) mutant: MutantResult,

    /// The file it belongs to.
    pub(super) file: String,

    /// When the run that produced this verdict started.
    pub(super) tested_at: u64,
}
