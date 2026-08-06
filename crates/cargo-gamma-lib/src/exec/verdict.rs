use camino::Utf8Path;
use core::time::Duration;
use std::io::BufReader;
use std::process::{ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use super::loader::{LOADER_VAR, UNDER_GAMMA_VAR, loader_path};
use super::memory::{Request, Usage};
use super::progress::Progress;
use super::subtree::{Subtree, contain};
use super::stall::Stall;
use super::test_binary::TestBinary;
use super::workspace::Workspace;

/// The variable libtest reads to size the stack of each spawned test thread.
const STACK_VAR: &str = "RUST_MIN_STACK";

/// The stack a test thread gets when the caller has not asked for a size, in bytes.
///
/// Eight times the usual two megabyte default, chosen to swallow the frame growth instrumentation
/// causes without being large enough to matter on a machine running many test processes at once,
/// since a thread stack is reserved lazily and only the pages actually touched are committed.
const STACK_FLOOR: usize = 16 * 1024 * 1024;

/// Returns the thread stack size to ask for, respecting a larger one the caller already chose.
fn stack_floor() -> String {
    let inherited = std::env::var(STACK_VAR).ok().and_then(|value| value.trim().parse::<usize>().ok());

    inherited.unwrap_or(0).max(STACK_FLOOR).to_string()
}

/// What running one test binary said.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Passed,

    /// A test failed, named when the harness said which.
    Failed(Option<String>),

    /// The budget ran out while the binary was still making progress.
    TimedOut,

    /// The binary stopped reporting progress long before its budget ran out.
    ///
    /// Carries the last test the harness said anything about, which is a landmark rather than a
    /// diagnosis: libtest runs tests in parallel and names one only when it finishes, so the test
    /// that is spinning is precisely the one it has not named.
    Stalled(Option<String>),

    /// The kernel stopped the test workload for passing the memory ceiling this run installed.
    ///
    /// A separate verdict rather than a kind of failure or a kind of timeout, because it is
    /// neither: the tests did not notice anything, and nothing ran out of time. What happened is
    /// that the mutant made the workload allocate past a ceiling the same workload stayed under
    /// with no mutant active. Carries the peak the platform observed, when it could observe one,
    /// and the ceiling that fired, since a reader's first question is how far past it went.
    MemoryLimit {
        /// The highest aggregate memory the subtree reached, when the platform reported one.
        peak: Option<u64>,

        /// The ceiling that was installed for this binary.
        limit: u64,
    },

    /// The accounting this run asked for could not be installed, so nothing was run.
    ///
    /// Not a verdict about the mutant at all. It exists so that a resource-control failure stops
    /// the run and says why, instead of quietly becoming an unprotected one.
    Unmetered(String),
}

/// Runs one test binary with an optional active mutant, under a wall-clock budget.
///
/// A verdict of [`Verdict::TimedOut`] or [`Verdict::Stalled`] is confirmed by a second run under a
/// budget several times larger. Both verdicts count as a detection, so a false one does not merely
/// lose information: it inflates the mutation score by crediting the suite with a kill it never
/// made. They are also the two verdicts a loaded machine can produce on its own — the budget is
/// calibrated from a baseline measured when nothing else was competing for cores, while mutants run
/// many at a time — so the first answer is treated as a suspicion and the second as the finding.
///
/// A suspected stall is retried with a looser silence budget rather than none at all, so that a
/// mutant that really has hung is still cut off early instead of waiting out the whole timeout.
pub(super) fn run_binary(
    work: &Workspace,
    binary: &TestBinary,
    active: Option<u32>,
    timeout: Duration,
    stall: Stall,
    request: Request,
) -> Verdict {
    let verdict = observe(work, binary, active, timeout, stall, request).verdict;

    match verdict {
        Verdict::TimedOut => observe(work, binary, active, timeout.saturating_mul(CONFIRM_FACTOR), stall, request).verdict,
        Verdict::Stalled(_) => observe(work, binary, active, timeout, stall.scaled(CONFIRM_FACTOR), request).verdict,
        // A memory verdict is deliberately not among the ones confirmed. The kernel observed the
        // cause rather than this code inferring it from a budget running out, so there is no
        // suspicion to settle — only a second runaway allocation to pay for.
        other => other,
    }
}

/// How much more room a suspected timeout or stall is given before it is believed.
///
/// Large enough that scheduling noise cannot survive it, and paid only by mutants that already
/// exhausted their budget — a small population, since a genuine hang is rare and a false one rarer
/// still.
pub const CONFIRM_FACTOR: u32 = 3;

/// Runs one binary, publishing the harness's progress into `progress` as it goes.
fn run_with(
    work: &Workspace,
    binary: &TestBinary,
    active: Option<u32>,
    timeout: Duration,
    stall: Stall,
    request: Request,
    progress: &Arc<Mutex<Progress>>,
) -> (Verdict, Usage) {
    let mut command = Command::new(binary.path.as_std_path());

    if let Some(path) = loader_path(&work.libraries) {
        let _ = command.env(LOADER_VAR, path);
    }

    // The harness reads these, so they have to precede nothing in particular but must be present
    // on every run — the baseline included, or the baseline would be measuring a different suite
    // from the one each mutant is judged against.
    let _ = command.args(&work.cargo.test_args);

    let _ = command
        .current_dir(working_directory(work, binary).as_std_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // A mutant that panics in a thousand tests would otherwise spend the whole budget
        // formatting backtraces nobody reads.
        .env("RUST_BACKTRACE", "0")
        // Every mutation site becomes a branch holding both the original and the replacement, so
        // an instrumented frame is larger than the one it stands in for. Deeply recursive code
        // that fits comfortably otherwise can exhaust a default 2 MiB test thread and abort the
        // process, which reads as a failure of the suite rather than of the mutant. Raising the
        // floor buys back the headroom instrumentation spent; an explicit setting still wins.
        .env(STACK_VAR, stack_floor())
        // Set for the baseline as well as for every mutant, so a test can branch on it without
        // knowing which phase it is in. A suite that shells out to cargo itself — this one
        // included — would otherwise run a nested build inside the scratch tree, failing for
        // reasons that have nothing to do with any mutant.
        .env(UNDER_GAMMA_VAR, "1");

    // Cargo sets this for every test it runs, and `env!("CARGO_MANIFEST_DIR")` is the usual way to
    // reach a fixture from a test. Left unset, the macro's compile-time value points into the
    // scratch tree's original location rather than the copy being tested.
    if !binary.manifest_dir.as_str().is_empty() {
        let _ = command.env("CARGO_MANIFEST_DIR", binary.manifest_dir.as_std_path());
    }

    match active {
        Some(ordinal) => {
            let _ = command.env(gamma_rt::ACTIVE_VAR, ordinal.to_string());
        }
        None => {
            let _ = command.env_remove(gamma_rt::ACTIVE_VAR);
        }
    }

    let guard = match contain(&mut command, request) {
        Ok(guard) => guard,
        Err(reason) => return (Verdict::Unmetered(reason), Usage::default()),
    };

    let Ok(mut child) = command.spawn() else {
        // A spawn that failed because the child could not join its accounting boundary is a setup
        // failure rather than a fact about the mutant, and reporting it as a failing test would
        // credit the suite with a kill it never made.
        return if request.wanted() {
            (
                Verdict::Unmetered(format!("`{}` could not be started inside its memory accounting boundary", binary.path)),
                Usage::default(),
            )
        } else {
            (Verdict::Failed(None), Usage::default())
        };
    };

    // Taken before anything is read from the child, so that the window in which a grandchild could
    // start outside the containment is as short as it can be made without owning the spawn.
    let subtree = match Subtree::adopt(&child, guard) {
        Ok(subtree) => subtree,
        Err(reason) => {
            // The child is already running outside the accounting the run believes it is inside,
            // so it is ended rather than left to allocate unwatched.
            let _killed = child.kill();
            let _reaped = child.wait();

            return (Verdict::Unmetered(reason), Usage::default());
        }
    };

    // The pipe must be drained by somebody other than the thread waiting for the child to exit. A
    // pipe holds about 64 KB; a test binary that prints more blocks forever in `write` while the
    // waiting thread sees a process that never finishes — turning a mutant that should time out
    // into one recorded as timed out for the wrong reason, or the baseline into a false ten-minute stall.
    //
    // The reader hands its text back over a channel rather than through a join, because the child
    // exiting does not guarantee the pipe is closed: anything the test spawned inherited the write
    // end, and a surviving grandchild holds it open indefinitely. Joining would then block forever
    // and take the whole run with it, so the wait is bounded and the reader is abandoned.
    let (sink, drained) = mpsc::channel::<Vec<u8>>();

    if let Some(pipe) = child.stdout.take() {
        let published = Arc::clone(progress);

        let _handle = thread::spawn(move || {
            let _sent = sink.send(drain(pipe, &published));
        });
    }

    let output = || drained.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = output();
                let usage = subtree.usage();

                if let Some(limit) = request.limit
                    && exhausted(&usage, limit, status.success())
                {
                    // Anything the workload spawned may still be alive: the kernel kills the
                    // cgroup as a unit, but a job object refuses allocations rather than
                    // necessarily killing, and either way the subtree is finished with.
                    subtree.kill(&mut child);

                    let _reaped = child.wait();

                    return (Verdict::MemoryLimit { peak: usage.peak, limit }, usage);
                }

                return if status.success() {
                    (Verdict::Passed, usage)
                } else {
                    (Verdict::Failed(first_failure(&String::from_utf8_lossy(&text))), usage)
                };
            }

            Ok(None) => {
                let stalled = stall.exceeded(progress);

                if stalled || Instant::now() >= deadline {
                    // Everything the test spawned goes too. An orphan holds locks in the scratch
                    // tree, which fails the next run, and an inherited pipe handle, which keeps
                    // whoever is reading this run's output from ever seeing end of file.
                    subtree.kill(&mut child);

                    let _ = child.wait();

                    let usage = subtree.usage();

                    // A workload thrashing against its ceiling runs out of time as well as out of
                    // memory, and the memory is the cause. Reporting the timeout instead would
                    // send the reader looking for a hang that is not there — and would spend a
                    // whole confirmation run reproducing the allocation.
                    if let Some(limit) = request.limit
                        && exhausted(&usage, limit, false)
                    {
                        return (Verdict::MemoryLimit { peak: usage.peak, limit }, usage);
                    }

                    // The text is not read on this path, so there is nothing to wait for.
                    return if stalled {
                        (Verdict::Stalled(last_test(progress)), usage)
                    } else {
                        (Verdict::TimedOut, usage)
                    };
                }

                thread::sleep(Duration::from_millis(5));
            }

            Err(_cause) => return (Verdict::Failed(None), subtree.usage()),
        }
    }
}

/// Whether a finished run should be read as having been stopped by its memory ceiling.
///
/// The platform's own report is the authority: on Linux an `oom` or `oom_kill` event recorded
/// against this invocation's cgroup, on Windows the job's accounting reaching the limit set for it.
/// A peak that merely touched the ceiling is not by itself a verdict, because reclaim may have
/// succeeded and the suite may have passed anyway — so a workload that succeeded is never convicted
/// however close it came, and a peak at the ceiling only counts when the workload also failed.
fn exhausted(usage: &Usage, limit: u64, succeeded: bool) -> bool {
    if succeeded {
        return false;
    }

    usage.exhausted || usage.peak.is_some_and(|peak| peak >= limit)
}

/// How long to wait for the reader to finish once the child has exited.
///
/// Normally the pipe reaches end of file the instant the child does, and the text is already in
/// hand. A wait this long is only ever reached when something the test spawned outlived it and
/// still holds the write end, in which case the text will never arrive and the alternative to
/// giving up is hanging.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// What one observed run of a test binary produced.
#[derive(Debug)]
pub(super) struct Observation {
    pub(super) verdict: Verdict,

    /// The longest the harness went quiet, for calibrating later runs.
    pub(super) quiet: Duration,

    /// How many tests the harness announced, or `None` if it announced nothing.
    pub(super) tests: Option<usize>,

    /// The highest aggregate memory the subtree reached, when the run asked for a measurement and
    /// the platform could supply one.
    pub(super) peak: Option<u64>,
}

/// The directory a test binary is launched from.
///
/// `cargo test` runs each binary with the working directory set to its package root, and tests
/// rely on it: a fixture opened as `tests/data/input.json` resolves from there and nowhere else. In
/// a single-package workspace the two are the same directory, which is why running everything from
/// the workspace root works until the day someone adds a second crate — and then every test that
/// touches a file fails identically with and without a mutant active, so every mutant in that
/// package is scored as a survivor.
///
/// Falls back to the workspace root when cargo did not say where the manifest was, which is the
/// behaviour that was always there.
fn working_directory<'work>(work: &'work Workspace, binary: &'work TestBinary) -> &'work Utf8Path {
    if binary.manifest_dir.as_str().is_empty() {
        &work.root
    } else {
        &binary.manifest_dir
    }
}

/// Runs one binary and reports what the harness said as well as how it ended.
pub(super) fn observe(
    work: &Workspace,
    binary: &TestBinary,
    active: Option<u32>,
    timeout: Duration,
    stall: Stall,
    request: Request,
) -> Observation {
    let progress = Arc::new(Mutex::new(Progress::new()));
    let (verdict, usage) = run_with(work, binary, active, timeout, stall, request, &progress);
    let quiet = quiet_of(&progress);

    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    let tests = progress.lock().unwrap().tests;

    Observation { verdict, quiet, tests, peak: usage.peak }
}

/// The last test the harness named, if any.
fn last_test(progress: &Mutex<Progress>) -> Option<String> {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().test.clone()
}

/// The longest silence a binary produced, for calibrating later runs.
fn quiet_of(progress: &Mutex<Progress>) -> Duration {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    let progress = progress.lock().unwrap();

    progress.quiet.max(Instant::now().saturating_duration_since(progress.heard))
}

/// How much of a test binary's output is worth keeping.
///
/// Only the first failure is ever read out of it, and libtest prints that early. A binary
/// producing more than this is extremely chatty or looping, and buffering it all would turn one
/// runaway mutant into an out-of-memory kill of the whole run.
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

/// Reads a child's output to exhaustion, keeping at most [`OUTPUT_CAP`] of it.
///
/// Reading continues past the cap even though the excess is discarded: the point is to keep the
/// pipe empty so the child can run to completion, not to collect the text.
fn drain(pipe: ChildStdout, progress: &Mutex<Progress>) -> Vec<u8> {
    use std::io::BufRead as _;

    let mut reader = BufReader::new(pipe);
    let mut kept = Vec::new();
    let mut line = Vec::new();

    loop {
        line.clear();

        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return kept,
            Ok(_read) => {
                // Published before the text is kept, so a binary past the cap still counts as
                // making progress. Silence is the signal, not volume.
                #[expect(clippy::unwrap_used, reason = "the watcher only panics if the process is unwinding")]
                progress.lock().unwrap().heard(&String::from_utf8_lossy(&line));

                let room = OUTPUT_CAP.saturating_sub(kept.len());

                if room > 0 {
                    kept.extend_from_slice(&line[..line.len().min(room)]);
                }
            }
        }
    }
}

/// Extracts the name of the first failing test from libtest's output.
fn first_failure(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("test ")?;
        let name = rest.strip_suffix(" ... FAILED")?;

        Some(name.trim().to_owned())
    })
}

/// Returns the last `count` lines of some text.
pub(super) fn tail(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);

    lines.get(start..).unwrap_or_default().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace whose test binaries are `/bin/sh`, running the given script.
    #[cfg(unix)]
    fn shell(body: &str) -> (tempfile::TempDir, Workspace) {
        crate::testing::shell_workspace("verdict", body)
    }

    /// A binary that cannot be started is a failure of that run, not of the whole session.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_cannot_be_spawned_is_a_plain_failure() {
        let (_directory, work) = shell("exit 0");
        let missing = crate::testing::test_binary(work.root.join("no-such-binary").as_str());

        // One test binary going missing should not panic or abort the run: the mutant it was
        // judging is simply recorded as having failed.
        assert_eq!(
            run_binary(&work, &missing, None, Duration::from_secs(1), Stall::NONE, Request::default()),
            Verdict::Failed(None)
        );
    }

    /// A binary that outlives its budget is timed out, and the verdict survives confirmation.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_outlives_its_budget_times_out() {
        let (_directory, work) = shell("sleep 30");
        let sleeper = crate::testing::test_binary("/bin/sh");

        // A genuine hang has to survive the confirmation run as well as the first one, so this
        // exercises both passes: the suspicion and the finding.
        assert_eq!(
            run_binary(&work, &sleeper, None, Duration::from_millis(50), Stall::NONE, Request::default()),
            Verdict::TimedOut
        );
    }

    /// A binary that goes quiet for longer than its stall budget is cut off early.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_goes_quiet_is_stalled_at_the_last_test_it_named() {
        let (_directory, work) = shell("echo 'test slow::case ... '; sleep 30");
        let hanger = crate::testing::test_binary("/bin/sh");

        // The point of stall detection is to cut a hang off long before the full budget, and to
        // say which test was running when the silence began.
        let verdict = run_binary(&work, &hanger, None, Duration::from_secs(60), Stall { budget: Some(Duration::from_millis(50)) }, Request::default());

        assert!(matches!(verdict, Verdict::Stalled(_)), "{verdict:?}");
    }

    /// A binary runs from its own package's root, the way cargo would run it.
    #[test]
    #[cfg(unix)]
    fn a_binary_runs_from_its_package_root() {
        // A test that opens a fixture by relative path only finds it from the package root. Run
        // from the workspace root it fails identically with and without a mutant active, so every
        // mutant in the package is scored as a survivor.
        let (_directory, work) = shell("test -f marker.txt");
        let package = work.root.join("crates").join("subject");

        std::fs::create_dir_all(package.as_std_path()).expect("the package directory is created");
        std::fs::write(package.join("marker.txt").as_std_path(), "here").expect("the marker is written");

        let binary = TestBinary {
            manifest_dir: package,
            ..crate::testing::test_binary("/bin/sh")
        };

        assert_eq!(
            run_binary(&work, &binary, None, Duration::from_secs(30), Stall::NONE, Request::default()),
            Verdict::Passed
        );
    }

    /// A binary that exits cleanly passes, and its output is read back.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_exits_cleanly_passes() {
        let (_directory, work) = shell("echo 'test a::b ... ok'; exit 0");
        let ok = crate::testing::test_binary("/bin/sh");

        assert_eq!(run_binary(&work, &ok, None, Duration::from_secs(30), Stall::NONE, Request::default()), Verdict::Passed);
    }

    /// A binary that exits non-zero fails, named by libtest's own report.
    #[test]
    #[cfg(unix)]
    fn a_failing_binary_is_named_by_its_first_failing_test() {
        let (_directory, work) = shell("echo 'test a::b ... FAILED'; exit 101");
        let bad = crate::testing::test_binary("/bin/sh");

        // The name comes out of the harness's own output rather than the exit status, which is
        // what makes a survivor report actionable.
        assert_eq!(
            run_binary(&work, &bad, Some(7), Duration::from_secs(30), Stall::NONE, Request::default()),
            Verdict::Failed(Some("a::b".to_owned()))
        );
    }

    /// A binary that allocates past its ceiling is a memory verdict, not a failing test.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_passes_its_ceiling_is_a_memory_verdict() {
        if super::super::memory::support().is_err() {
            return;
        }

        // Shared memory rather than a file on disk, because page cache backed by a disk is
        // reclaimable and the workload would stay under the ceiling indefinitely instead of
        // crossing it.
        let fill = format!("/dev/shm/gamma-verdict.{}", std::process::id());
        let (_directory, work) = shell(&format!("dd if=/dev/zero of={fill} bs=1M count=512 2>/dev/null"));
        let greedy = crate::testing::test_binary("/bin/sh");
        let limit = 32 * 1024 * 1024;

        let verdict = run_binary(
            &work,
            &greedy,
            Some(1),
            Duration::from_secs(60),
            Stall::NONE,
            Request { meter: true, limit: Some(limit) },
        );

        let _removed = std::fs::remove_file(&fill);

        // Reporting this as a plain failure would be defensible and wrong: the suite noticed
        // nothing, the kernel did, and only the kernel's report distinguishes it from the
        // ordinary case of a test that exits non-zero.
        assert!(matches!(verdict, Verdict::MemoryLimit { .. }), "{verdict:?}");
    }

    /// A binary that stays under its ceiling is judged by its tests, not by its allocations.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_stays_under_its_ceiling_is_judged_normally() {
        if super::super::memory::support().is_err() {
            return;
        }

        let (_directory, work) = shell("echo 'test a::b ... ok'; exit 0");
        let modest = crate::testing::test_binary("/bin/sh");

        // The expensive mistake in the other direction: a ceiling that convicts a healthy mutant
        // credits the suite with a kill it never made and inflates the score.
        assert_eq!(
            run_binary(
                &work,
                &modest,
                None,
                Duration::from_secs(30),
                Stall::NONE,
                Request { meter: true, limit: Some(512 * 1024 * 1024) }
            ),
            Verdict::Passed
        );
    }

    /// A run that reached its ceiling and still passed is not convicted by the peak alone.
    #[test]
    fn a_successful_run_is_never_convicted_by_its_peak() {
        // On Linux a peak can sit exactly at the ceiling because reclaim did its job. The suite
        // passed; the mutant was not caught by anything, and saying otherwise would be a
        // detection invented out of an accounting figure.
        let usage = Usage { peak: Some(1024), exhausted: false };

        assert!(!exhausted(&usage, 1024, true));
        assert!(exhausted(&usage, 1024, false));
        assert!(!exhausted(&usage, 2048, false));
    }

    /// The kernel's own report convicts even when the peak reads below the ceiling.
    #[test]
    fn a_kernel_reported_kill_is_believed_whatever_the_peak_says() {
        // `memory.peak` is a high-water mark sampled by the kernel and an OOM kill can free the
        // charge before it is read, so the event is the authority and the peak is the detail.
        let usage = Usage { peak: Some(1), exhausted: true };

        assert!(exhausted(&usage, u64::MAX, false));
        assert!(!exhausted(&usage, u64::MAX, true));
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a\nb", 10), "a\nb");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn the_first_libtest_failure_name_is_extracted() {
        let output = "running 2 tests\n\
                      test passing ... ok\n\
                      test module::case ... FAILED\n\
                      test later ... FAILED\n";

        // Only the first failing test is reported with a killed mutant; later failures may be
        // consequences of the same defect and add noise.
        assert_eq!(first_failure(output), Some("module::case".to_owned()));
        assert_eq!(first_failure("custom harness output"), None);
    }
}
