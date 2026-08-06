use std::sync::Mutex;
use std::time::Instant;
use core::time::Duration;

use super::progress::Progress;

/// How long a binary may go silent before it is presumed hung.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stall {
    /// The budget, or `None` to wait out the full timeout.
    pub budget: Option<Duration>,
}

impl Stall {
    /// No stall detection: every mutant waits out its whole budget.
    pub(super) const NONE: Self = Self { budget: None };

    /// Builds a budget from the longest silence the baseline legitimately produced.
    ///
    /// Calibrating from the measured quiet period is the point: a suite whose slowest test takes
    /// half a minute goes quiet that long when healthy, and a fixed budget would either call that
    /// a hang or be too loose to help a suite of millisecond tests.
    #[must_use]
    pub fn calibrated(quiet: Duration, factor: f64, floor: Duration, ceiling: Duration) -> Self {
        let budget = quiet.mul_f64(factor).max(floor).min(ceiling);

        Self { budget: Some(budget) }
    }

    /// Whether the binary has been silent for longer than the budget allows.
    pub(super) fn exceeded(self, progress: &Mutex<Progress>) -> bool {
        let Some(budget) = self.budget else {
            return false;
        };

        #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
        let progress = progress.lock().unwrap();

        Instant::now().saturating_duration_since(progress.heard) > budget
    }

    /// The same budget, multiplied.
    ///
    /// Used to re-ask a question rather than to ask a new one: a suspected stall is retried under a
    /// budget this much looser, which scheduling noise cannot survive but a real hang still will.
    pub(super) fn scaled(self, factor: u32) -> Self {
        Self { budget: self.budget.map(|budget| budget.saturating_mul(factor)) }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn a_stall_budget_scales_with_the_silence_the_baseline_produced() {
        let quiet = Duration::from_secs(30);
        let stall = Stall::calibrated(quiet, 2.0, Duration::from_secs(1), Duration::from_secs(600));

        assert_eq!(stall.budget, Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_suite_that_never_goes_quiet_still_gets_a_usable_budget() {
        // Otherwise a suite of millisecond tests would produce a budget of nothing, and scheduler
        // noise on a loaded machine would read as a hang.
        let stall = Stall::calibrated(Duration::ZERO, 10.0, Duration::from_secs(5), Duration::from_secs(600));

        assert_eq!(stall.budget, Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_stall_budget_never_exceeds_the_timeout_it_is_meant_to_pre_empt() {
        let stall = Stall::calibrated(Duration::from_secs(300), 10.0, Duration::ZERO, Duration::from_secs(60));

        assert_eq!(stall.budget, Some(Duration::from_secs(60)));
    }

    #[test]
    fn without_a_budget_nothing_is_ever_declared_stalled() {
        let progress = Mutex::new(Progress::new());

        thread::sleep(Duration::from_millis(20));

        assert!(!Stall::NONE.exceeded(&progress));
    }

    #[test]
    fn silence_beyond_the_budget_is_a_stall() {
        let progress = Mutex::new(Progress::new());
        let stall = Stall { budget: Some(Duration::from_millis(5)) };

        thread::sleep(Duration::from_millis(30));

        assert!(stall.exceeded(&progress));
    }

    #[test]
    fn a_line_of_output_clears_the_silence() {
        let progress = Mutex::new(Progress::new());
        let stall = Stall { budget: Some(Duration::from_millis(50)) };

        thread::sleep(Duration::from_millis(60));

        progress.lock().expect("a test holds the only reference").heard("test tests::a ... ok\n");

        assert!(!stall.exceeded(&progress));
    }
}
