use core::time::Duration;
use std::time::Instant;

use crate::Result;
use crate::error::error;

use super::memory::Request;
use super::stall::Stall;
use super::test_binary::TestBinary;
use super::verdict::{Verdict, observe};
use super::workspace::Workspace;

/// What the baseline measured.
#[derive(Debug, Clone, Copy)]
pub(super) struct Baseline {
    /// How long the suite took.
    pub(super) elapsed: Duration,

    /// The longest the suite legitimately went quiet, which calibrates the stall budget.
    pub(super) quiet: Duration,

    /// How many tests ran, or `None` if no harness said.
    ///
    /// This is what ran, not what exists: `--test-package`, `--test-workspace` and any filter
    /// passed through to the harness all narrow it. That is the useful figure, since it is exactly
    /// the set of tests that will pass judgement on every mutant.
    pub(super) tests: Option<usize>,

    /// The largest peak memory any one binary reached, when the run asked for a measurement.
    ///
    /// The whole suite's peaks are not added up, because the binaries run one at a time: what a
    /// ceiling has to admit is the most any single one of them needed, not the sum of what all of
    /// them needed at different moments.
    pub(super) peak: Option<u64>,
}

/// How long one binary gets to produce a baseline before the run gives up on it.
///
/// Generous rather than calibrated, because there is nothing to calibrate from yet: this is the
/// measurement every later budget is derived from. It only ever fires on a suite that has hung.
const BASELINE_BUDGET: Duration = Duration::from_secs(600);

/// Runs the suite with no mutant active and returns how long it took.
pub(super) fn measure_baseline(work: &Workspace, binaries: &mut [TestBinary], request: Request) -> Result<Baseline> {
    measure_within(work, binaries, BASELINE_BUDGET, request)
}

/// Measures the baseline under an explicit budget.
///
/// The budget is a parameter so that the paths a hung or failing suite takes can be exercised
/// without waiting out the real one.
fn measure_within(work: &Workspace, binaries: &mut [TestBinary], budget: Duration, request: Request) -> Result<Baseline> {
    let started = Instant::now();
    let mut quiet = Duration::ZERO;
    let mut tests: Option<usize> = None;
    let mut peak: Option<u64> = None;

    for entry in binaries.iter_mut() {
        let began = Instant::now();
        let observed = observe(work, entry, None, budget, Stall::NONE, request);

        entry.baseline = began.elapsed();
        entry.peak = observed.peak;
        quiet = quiet.max(observed.quiet);

        if let Some(measured) = observed.peak {
            peak = Some(peak.unwrap_or(0).max(measured));
        }

        // A binary with no harness contributes nothing rather than turning the total into a
        // guess, but one binary reporting is enough for the total to be worth stating.
        if let Some(counted) = observed.tests {
            tests = Some(tests.unwrap_or(0).saturating_add(counted));
        }

        match observed.verdict {
            Verdict::Passed => {}
            Verdict::Failed(name) => {
                let which = name.map_or_else(|| "a test".to_owned(), |test| format!("test `{test}`"));
                let path = &entry.path;

                return Err(error!(
                    "the baseline is not green: {which} in `{path}` fails before any mutant is applied.\n\
                     Every verdict in a run is a comparison against the baseline, so there is nothing to \
                     measure until the suite passes."
                ));
            }
            Verdict::TimedOut | Verdict::Stalled(_) => {
                return Err(baseline_timeout_error(&entry.path));
            }
            Verdict::MemoryLimit { peak, limit } => {
                return Err(baseline_memory_error(&entry.path, peak, limit));
            }
            Verdict::Unmetered(reason) => {
                return Err(error!(
                    "the memory accounting this run asked for could not be installed: {reason}.\n\
                     Nothing was measured under it, so the run stops here rather than continue \
                     without the protection it was asked for."
                ));
            }
        }
    }

    Ok(Baseline { elapsed: started.elapsed(), quiet, tests, peak })
}

fn baseline_timeout_error(binary: &camino::Utf8Path) -> crate::error::Error {
    error!("the baseline run of `{binary}` did not finish within ten minutes")
}

/// Explains a baseline that hit the explicit ceiling put around the calibration itself.
///
/// A ceiling derived from the baseline cannot protect the machine from a baseline that is itself
/// runaway, so a ceiling may be placed around the calibration. When that one fires, no mutant is
/// involved: the suite needs more memory than the run was told to allow it, and the number to
/// change is the one the user supplied.
fn baseline_memory_error(binary: &camino::Utf8Path, peak: Option<u64>, limit: u64) -> crate::error::Error {
    let reached = peak.map_or_else(String::new, |peak| format!(", reaching {}", crate::report::bytes(peak)));

    error!(
        "the baseline run of `{binary}` exceeded the {} it was allowed{reached}.\n\
         Every mutant is judged against this run, so there is nothing to measure until the suite \
         fits within the ceiling `--baseline-memory-limit` set, or that ceiling is raised.",
        crate::report::bytes(limit)
    )
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    use super::*;

    /// Wraps an existing directory as a workspace whose "test binary" is `/bin/sh`.
    ///
    /// The script is passed as an argument rather than written to disk: a file made executable
    /// while other threads are forking can be refused with `ETXTBSY`, which would make these
    /// tests intermittently fail for a reason that has nothing to do with what they assert.
    #[cfg(unix)]
    fn harness(body: &str) -> (tempfile::TempDir, Workspace, Vec<TestBinary>) {
        let (directory, work) = crate::testing::shell_workspace("baseline", body);
        let binaries = vec![TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::test_binary("/bin/sh")
        }];

        (directory, work, binaries)
    }

    /// A suite that hangs before any mutant is applied stops the run.
    #[test]
    #[cfg(unix)]
    fn a_baseline_that_never_finishes_stops_the_run() {
        let (_directory, work, mut binaries) = harness("sleep 30");
        let failure =
            measure_within(&work, &mut binaries, Duration::from_millis(50), Request::default()).expect_err("the baseline must fail");

        // Continuing would time out every mutant against a suite that never finishes and report a
        // perfect score built entirely out of false detections.
        assert!(failure.to_string().contains("did not finish"), "{failure}");
    }

    /// A suite that is already failing stops the run, naming the test.
    #[test]
    #[cfg(unix)]
    fn a_red_baseline_stops_the_run_and_names_the_failing_test() {
        let (_directory, work, mut binaries) = harness("echo 'test a::b ... FAILED'\nexit 101");
        let failure =
            measure_within(&work, &mut binaries, Duration::from_secs(30), Request::default()).expect_err("the baseline must fail");

        // Every verdict is a comparison against the baseline, so a red one makes every mutant
        // look killed by a failure that was there before mutation started.
        assert!(failure.to_string().contains("test `a::b`"), "{failure}");
    }

    /// A green suite yields the elapsed time and the harness's own test count.
    #[test]
    #[cfg(unix)]
    fn a_green_baseline_reports_the_elapsed_time_and_the_test_count() {
        let (_directory, work, mut binaries) = harness("echo 'running 3 tests'\necho 'test a::b ... ok'\nexit 0");
        let baseline =
            measure_within(&work, &mut binaries, Duration::from_secs(30), Request::default()).expect("the baseline must pass");

        // The per-binary baseline is what apportions each mutant's budget, so it has to be
        // written back rather than merely totalled.
        assert_eq!(baseline.tests, Some(3));
        assert!(binaries[0].baseline > Duration::ZERO);
    }

    /// A binary whose harness announces nothing still contributes its time.
    #[test]
    #[cfg(unix)]
    fn a_baseline_with_no_harness_count_still_measures_the_time() {
        let (_directory, work, mut binaries) = harness("echo 'custom harness'\nexit 0");
        let baseline =
            measure_within(&work, &mut binaries, Duration::from_secs(30), Request::default()).expect("the baseline must pass");

        // A custom harness is not a broken one; inventing a count would be worse than omitting it.
        assert_eq!(baseline.tests, None);
    }

    /// A metered baseline writes each binary's peak back, which is what a ceiling is derived from.
    #[test]
    #[cfg(unix)]
    fn a_metered_baseline_records_what_each_binary_used() {
        if crate::exec::memory::support().is_err() {
            return;
        }

        let (_directory, work, mut binaries) = harness("dd if=/dev/zero of=/dev/null bs=1M count=32 2>/dev/null\nexit 0");
        let request = Request { meter: true, limit: None };
        let baseline =
            measure_within(&work, &mut binaries, Duration::from_secs(30), request).expect("the baseline must pass");

        // A ceiling is derived per binary, so the per-binary figure has to be written back and not
        // merely totalled; a run that only kept the total would bound every binary by the largest.
        assert!(binaries[0].peak.is_some(), "{:?}", binaries[0].peak);
        assert_eq!(baseline.peak, binaries[0].peak);
    }

    /// A baseline that outgrows its own explicit ceiling stops the run and says which number to move.
    #[test]
    fn a_baseline_that_outgrows_its_ceiling_names_the_ceiling() {
        let cause = baseline_memory_error(Utf8Path::new("/workspace/target/debug/deps/unit"), Some(300 * 1024 * 1024), 256 * 1024 * 1024)
            .to_string();

        // The user set this ceiling themselves, and no mutant is involved, so the message has to
        // point at the flag rather than read like a mutant was caught.
        assert!(cause.contains("unit"), "{cause}");
        assert!(cause.contains("--baseline-memory-limit"), "{cause}");
        assert!(cause.contains("256.0 MB"), "{cause}");
        assert!(cause.contains("300.0 MB"), "{cause}");
    }

    #[test]
    fn a_baseline_timeout_names_the_binary_that_stopped_progress() {
        let cause = baseline_timeout_error(Utf8Path::new("/workspace/target/debug/deps/unit")).to_string();

        // A timeout before mutants run is a property of the fixed suite, so the message must point
        // at the binary the user can run directly.
        assert!(cause.contains("unit"), "{cause}");
        assert!(cause.contains("ten minutes"), "{cause}");
    }
}
