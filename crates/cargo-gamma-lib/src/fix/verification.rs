/// The outcome of verifying a set of edits against a fresh discovery.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verification {
    /// Mutants that were meant to be suppressed and are not.
    pub missing: Vec<String>,

    /// Mutants that became suppressed and were not meant to be.
    ///
    /// The dangerous half. A directive attached to a multi-line construct takes out everything
    /// inside it, and if any of those were survivors the guarantee at the top of this module has
    /// been violated by accident.
    pub collateral: Vec<String>,
}

impl Verification {
    /// Whether the edit may stand.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.collateral.is_empty()
    }
}
