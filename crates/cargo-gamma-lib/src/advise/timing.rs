use core::time::Duration;

/// What a run cost, beyond the verdicts.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long the single instrumented build took.
    pub build: Duration,

    /// How long the suite took with no mutant active.
    pub baseline: Duration,

    /// Total wall time for the whole run.
    pub wall: Duration,

    /// How many mutants were tested at once.
    pub jobs: usize,
}
