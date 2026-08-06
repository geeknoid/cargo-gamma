use std::time::Instant;
use core::time::Duration;

/// What the harness has said so far, published by the reader for the waiting thread to watch.
#[derive(Debug)]
pub(super) struct Progress {
    /// When the harness last said anything at all.
    pub(super) heard: Instant,

    /// Whether the harness has produced any output yet.
    heard_any: bool,

    /// The longest silence so far.
    ///
    /// Calibrated from the baseline, this is how long the suite legitimately goes quiet while its
    /// slowest test runs, and it becomes the basis for the stall budget.
    pub(super) quiet: Duration,

    /// The last test the harness named.
    pub(super) test: Option<String>,

    /// How many tests the harness said it was about to run, summed over every suite in the binary.
    ///
    /// A binary holds one suite per `#[cfg(test)]` module tree plus the doc tests, and each
    /// announces itself separately, so the figure is a running total rather than the last one
    /// seen. `None` until something announces anything: a target built with `harness = false`
    /// prints whatever it likes and must contribute nothing rather than a confident zero.
    pub(super) tests: Option<usize>,
}

impl Progress {
    pub(super) fn new() -> Self {
        Self {
            heard: Instant::now(),
            heard_any: false,
            quiet: Duration::ZERO,
            test: None,
            tests: None,
        }
    }

    /// Records that the harness produced a line, and how long it had been silent beforehand.
    pub(super) fn heard(&mut self, line: &str) {
        let now = Instant::now();

        if self.heard_any {
            self.quiet = self.quiet.max(now.saturating_duration_since(self.heard));
        }

        self.heard = now;
        self.heard_any = true;

        if let Some(rest) = line.strip_prefix("test ")
            && let Some((name, _verdict)) = rest.split_once(" ... ")
        {
            self.test = Some(name.trim().to_owned());

            return;
        }

        // libtest announces each suite with `running N tests`, which is the only place the size of
        // the run is stated. Counting the result lines instead would miss tests that were filtered
        // out and would double-count a harness that reports progress more than once.
        if let Some(count) = line
            .trim()
            .strip_prefix("running ")
            .and_then(|rest| rest.strip_suffix(" tests").or_else(|| rest.strip_suffix(" test")))
            .and_then(|count| count.trim().parse::<usize>().ok())
        {
            self.tests = Some(self.tests.unwrap_or(0).saturating_add(count));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn the_harness_naming_a_test_records_it() {
        let mut progress = Progress::new();

        progress.heard("test tests::the_boundary_is_pinned ... ok\n");

        assert_eq!(progress.test.as_deref(), Some("tests::the_boundary_is_pinned"));
    }

    #[test]
    fn a_line_that_is_not_a_test_result_does_not_rename_the_test() {
        let mut progress = Progress::new();

        progress.heard("test tests::first ... ok\n");
        progress.heard("running 3 tests\n");
        progress.heard("some output from the test itself\n");

        assert_eq!(progress.test.as_deref(), Some("tests::first"));
    }

    #[test]
    fn the_size_of_each_suite_is_summed() {
        // One binary announces its unit tests and its doc tests separately.
        let mut progress = Progress::new();

        progress.heard("running 12 tests\n");
        progress.heard("test a ... ok\n");
        progress.heard("running 3 tests\n");

        assert_eq!(progress.tests, Some(15));
    }

    #[test]
    fn a_single_test_is_announced_in_the_singular() {
        let mut progress = Progress::new();

        progress.heard("running 1 test\n");

        assert_eq!(progress.tests, Some(1));
    }

    #[test]
    fn an_empty_suite_still_counts_as_having_reported() {
        // `running 0 tests` is what every target without tests prints, and is not the same as a
        // harness that said nothing at all.
        let mut progress = Progress::new();

        progress.heard("running 0 tests\n");

        assert_eq!(progress.tests, Some(0));
    }

    #[test]
    fn a_harness_that_announces_nothing_reports_no_count() {
        // A target with `harness = false` prints whatever it likes. Guessing zero would understate
        // a suite that really did run.
        let mut progress = Progress::new();

        progress.heard("Running my own tests\n");
        progress.heard("all good\n");

        assert_eq!(progress.tests, None);
    }

    #[test]
    fn the_longest_silence_is_the_one_remembered() {
        let mut progress = Progress::new();

        progress.heard("started\n");
        thread::sleep(Duration::from_millis(30));
        progress.heard("a\n");
        let long = progress.quiet;

        progress.heard("b\n");

        assert_eq!(progress.quiet, long, "a short gap must not replace a long one");
        assert!(long >= Duration::from_millis(25), "{long:?}");
    }

    #[test]
    fn process_startup_does_not_calibrate_test_silence() {
        let mut progress = Progress::new();

        thread::sleep(Duration::from_millis(30));
        progress.heard("running 1 test\n");

        assert_eq!(progress.quiet, Duration::ZERO);
    }
}
