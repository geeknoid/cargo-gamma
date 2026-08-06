//! Building the mutant schema once and running the test suite against each mutant.
//!
//! A run copies the workspace to a scratch tree, instruments every mutated file, builds the test
//! binaries once, measures a baseline with no mutant active, then runs the suite once per mutant
//! with `GAMMA_ACTIVE` naming the one that is live. Every test process, baseline included, gets
//! `CARGO_GAMMA=1` so a suite that drives cargo itself can opt out of a nested build.
//!
//! Mutants that cannot compile — replacing a return value with `Default::default()` when the type
//! is not `Default`, for instance — are attributed back to the guards that caused them, withdrawn,
//! and the build is retried; a handful of rollback rounds converges.

mod baseline;
mod build;
mod cargo_options;
#[cfg(target_os = "linux")]
mod cgroup;
mod config;
mod copy;
mod events;
mod loader;
mod manifest;
// Exposed to integration tests, which need to ask whether this host can bound memory at all before
// they can say what a run should have done. See `declare_modules!` in `lib.rs` for the convention.
#[cfg(feature = "internals")]
pub mod memory;
#[cfg(not(feature = "internals"))]
mod memory;
mod progress;
mod session;
mod stall;
mod test_binary;
mod verdict;
mod subtree;
mod workspace;

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::Instant;

use crate::Result;
use crate::discover::{Plan, Survey};
use crate::error::error;
use crate::estimate::project;
use crate::model::Outcome;
use crate::ops::registry::Selection;

use baseline::{Baseline, measure_baseline};
use build::Converger;
use memory::Request;
use stall::Stall;
use test_binary::{TestScope, apportion, bound, build_packages, reaches, restrict, unmatched_test, workload};
use verdict::{Verdict, run_binary};

pub use cargo_options::{BuildLimits, CargoOptions, DEFAULT_ROLLBACK_ROUNDS};
pub use config::Config;
pub use events::Events;
pub use loader::UNDER_GAMMA_VAR;
pub use memory::{DEFAULT_HEADROOM, DEFAULT_MULTIPLIER, Demand, MemoryControl, MemoryPolicy};
pub use session::Session;
pub use test_binary::TestBinary;
pub use workspace::{Workspace, scratch_tree};
pub(crate) use verdict::CONFIRM_FACTOR;

/// Runs every live mutant in the plan, writing verdicts back onto it.
///
/// # Errors
///
/// Returns an error if the tree cannot be prepared, the build cannot be made to succeed, or the
/// baseline does not pass — a failing baseline means every comparison in the run has nothing to
/// compare against.
pub fn run(survey: &Survey, selection: &Selection, config: &Config, events: &mut impl Events) -> Result<Measured> {
    let Measured { mut plan, built } = measure(survey, selection, config, events)?;

    // Nothing was live, so nothing was copied, built or measured. The plan still describes every
    // mutant that was found and why each one is not being run, which is what the caller reports.
    let Some(built) = built else {
        return Ok(Measured { plan, built: None });
    };

    let stall = Stall { budget: built.session.stall };

    let scope = TestScope {
        packages: &config.test_packages,
        whole_workspace: config.test_workspace,
    };

    let (reachable, budget) = workload(&plan.mutants, &built.session.binaries, &plan, &scope);
    let projection = project(&plan.mutants, reachable, budget, built.session.baseline, built.session.build, config.jobs);

    events.measured(&plan, &built.session, &projection);

    let sweep = Sweep { stall, jobs: config.jobs, meter: built.session.metered };

    test_all(&built.work, &mut plan, &built.session.binaries, sweep, scope, events)?;

    Ok(Measured { plan, built: Some(built) })
}

/// What a run worked out before testing a single mutant.
#[derive(Debug)]
pub struct Measured {
    /// Every mutant that was found, whether or not it will be run.
    pub plan: Plan,

    /// The tree and the measurements, absent when there was nothing live to build for.
    pub built: Option<Built>,
}

/// A built tree and what measuring it revealed.
#[derive(Debug)]
pub struct Built {
    /// The scratch tree. It owns the built binaries, so dropping it deletes them.
    pub work: Workspace,

    /// The timings and test binaries the run works from.
    pub session: Session,
}

/// Scans, instruments, builds and measures the baseline, without testing a single mutant.
///
/// Each package is taken from source to compiled object before the next one starts, in an order
/// where a package always follows what it depends on. That order is forced anyway — a mutant
/// cannot produce a diagnostic until everything its own package depends on compiles clean — and
/// following it deliberately lets a run say which package it is working on, and say it once, before
/// the wait rather than after it.
///
/// The tree is copied on demand rather than up front. A run whose every mutant turns out to be
/// suppressed has nothing to build, and it should not pay for a copy to discover that.
///
/// # Errors
///
/// Returns an error if a file cannot be parsed, the tree cannot be prepared, the build cannot be
/// made to succeed, or the baseline does not pass — a failing baseline means every comparison in
/// the run has nothing to compare against.
pub fn measure(survey: &Survey, selection: &Selection, config: &Config, events: &mut impl Events) -> Result<Measured> {
    let started = Instant::now();

    let (memory, unbounded) = admit_memory_control(config)?;

    // Checked against what the workspace declares, before anything is copied or compiled. A typo
    // here changes which tests get to convict a mutant, so it should cost a second rather than a
    // full instrumented build.
    if let Some(pattern) = unmatched_test(&survey.tests, &config.include_tests, &config.exclude_tests) {
        return Err(error!("no test target matches `{pattern}`; patterns match cargo target names, not test function names").usage());
    }

    let mut plan = survey.skeleton();
    let mut converger = Converger::default();
    let mut ordinals = 0_u32;
    let mut anything_live = false;
    let work = Workspace::prepare(&plan.root, config, events)?;

    for stage in &crate::discover::stages(&survey.packages(), &survey.reach) {
        let name = stage.join(", ");

        // Named on the way in, before its files have even been read. Scanning and then compiling a
        // large crate is the longest a run goes without saying anything, and what makes that wait
        // legible is knowing whose wait it is.
        events.begin("Processing", &name);

        let mut live = 0_usize;

        for package in stage {
            let scanned = survey.scan(Some(package), selection, &mut ordinals)?;

            live = live.saturating_add(scanned.mutants.iter().filter(|mutant| mutant.ordinal > 0).count());
            plan.absorb(scanned);
        }

        // A package with nothing to run is still named. A crate that quietly takes no part in a run
        // is worth noticing, and leaving it out of the sequence would make it look like it had
        // simply not been looked at.
        if live == 0 {
            events.end(", no mutants");
            continue;
        }

        for package in stage {
            work.link_runtime(package, &plan.files)?;
        }

        let before = converger.withdrawn();

        converger.stage(&work, &plan, stage, config.build)?;
        anything_live = true;

        // The count that closes the line is what survived compilation, which is why it waits for
        // the build. A mutant that could not compile is a fact about the tool rather than about the
        // code, and the summary accounts for all of them once.
        let viable = live.saturating_sub(converger.withdrawn().saturating_sub(before));

        events.end(&format!(", {}", crate::report::quantity(viable, "viable mutant")));
    }

    plan.sort();

    // Nothing was live anywhere, so there is nothing to build, measure or run.
    if !anything_live {
        return Ok(Measured { plan, built: None });
    }

    // One line for the whole fixed cost that is left. The test binaries are built and then
    // immediately run with no mutant active, and neither half means anything without the other:
    // the build is what makes a baseline possible, and the baseline is what says the build was
    // worth having.
    events.begin("Baseline", "building the test binaries and running the suite");

    // The staged builds compiled libraries only. This is the build that compiles the test targets
    // and settles the run, and it withdraws whatever only a test target could have revealed. Only
    // the packages whose tests can actually be selected are asked for: the rest would be compiled,
    // baselined and never consulted.
    let scope = TestScope {
        packages: &config.test_packages,
        whole_workspace: config.test_workspace,
    };

    let select = build_packages(&plan, &scope);
    let mut build = converger.finish(&work, &mut plan, select.as_deref(), config.build)?;

    // Before the baseline, so the shares `apportion` computes describe the suite that will actually
    // run. A run with nothing left to run it cannot decide anything: every mutant would survive
    // unopposed and the report would read as a total failure of the test suite rather than as the
    // filter having eaten it.
    let present = build.binaries.len();

    restrict(&mut build.binaries, &config.include_tests, &config.exclude_tests);

    let filtered = present.saturating_sub(build.binaries.len());

    if build.binaries.is_empty() && present > 0 {
        return Err(error!("`--include-test` and `--exclude-test` left no test target to decide a verdict").usage());
    }

    let build_time = started.elapsed();
    let baseline = if config.baseline {
        let measured = measure_baseline(
            &work,
            &mut build.binaries,
            Request {
                meter: memory.measuring(),
                limit: memory.baseline_limit,
            },
        )?;

        // Reported after the fact rather than before, because the two figures everything
        // downstream is derived from — the mutant timeout is this elapsed time scaled by
        // `--timeout-multiplier`, and the stall budget is calibrated from the longest silence
        // within it — only exist once the suite has actually run.
        events.end(&format!(", {}", describe(&measured)));

        measured
    } else {
        events.end(", no baseline was measured");

        Baseline { elapsed: Duration::ZERO, quiet: Duration::ZERO, tests: None, peak: None }
    };

    let timeout = config.timeout.unwrap_or_else(|| {
        let scaled = baseline.elapsed.mul_f64(config.timeout_multiplier);

        scaled.max(config.timeout_floor)
    });

    // Without a baseline there is no calibration, so a stall cannot be detected and every mutant
    // waits out its whole budget.
    let stall = if config.stall && config.baseline {
        Stall::calibrated(baseline.quiet, config.stall_factor, config.stall_floor, timeout)
    } else {
        Stall::NONE
    };

    // Cheapest first: the loop stops at the first binary that fails, so trying the quick ones
    // first makes a kill cost less. It changes no verdict, only what a verdict costs.
    build.binaries.sort_by_key(|binary| binary.baseline);

    apportion(&mut build.binaries, timeout, config.timeout_floor);
    bound(&mut build.binaries, &memory, config.baseline);

    let session = Session {
        baseline: baseline.elapsed,
        quiet: baseline.quiet,
        stall: stall.budget,
        timeout,
        build: build_time,
        peak: baseline.peak,
        metered: memory.measuring(),
        unbounded,
        withdrawn: build.withdrawn,
        rounds: build.rounds,
        binaries: build.binaries,
        footprint: work.footprint(),
        filtered,
        not_built: plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::NotBuilt).count(),
        widened: build.widened,
    };

    // Everything that could have failed has. What was built is now worth keeping, so that the next
    // run in this workspace is incremental rather than starting cold.
    work.settle();

    Ok(Measured { plan, built: Some(Built { work, session }) })
}

/// Settles what memory control this run can actually deliver, refusing or degrading as appropriate.
///
/// Two things can make the configured policy impossible: a host with no way to account for a whole
/// process tree, and a run with no baseline to calibrate a ceiling from. What happens then depends
/// on who asked. Someone who passed `--memory` did so because an unbounded mutant would cost them
/// something, and a run that quietly gave them nothing would be discovered only by the thing they
/// were trying to prevent — so that is an error. Someone who passed nothing has the default, and
/// refusing to produce a mutation score because this machine has no cgroup delegation would be an
/// obstruction rather than a safeguard — so that degrades to no memory control, out loud.
///
/// It is said out loud rather than silently because the protection is the kind whose absence is
/// invisible until it matters. A user who believes their machine is protected and finds out
/// otherwise mid-run is worse off than one who was told plainly at the start.
fn admit_memory_control(config: &Config) -> Result<(MemoryPolicy, Option<String>)> {
    let policy = config.memory;

    if !policy.measuring() {
        return Ok((policy, None));
    }

    if let Err(reason) = memory::support() {
        if policy.insisted() {
            return Err(error!(
                "memory control was asked for, but it is not available here: {reason}.\n\
                 Run with `--memory off` to continue without it."
            ));
        }

        return Ok((policy.disabled(), Some(reason)));
    }

    // A ceiling is calibrated from the baseline. Without one there is no measurement to calibrate
    // from, and a number invented here would be presented with exactly the confidence of a
    // measured one.
    if policy.enforcing() && !config.baseline && policy.limit.is_none() {
        if policy.insisted() {
            return Err(error!(
                "`--memory enforce` derives each test binary's ceiling from what it used during the \
                 baseline, and `--no-baseline` means there is no such measurement.\n\
                 Pass `--memory-limit` to state a ceiling outright, or drop `--no-baseline`."
            ));
        }

        return Ok((
            policy.disabled(),
            Some("`--no-baseline` leaves no measurement to derive a ceiling from".to_owned()),
        ));
    }

    Ok((policy, None))
}

/// Describes a mutant stopped by the memory ceiling installed for its test binary.
///
/// Written to say what was measured and against what, because the reader's questions are whether
/// the ceiling was reasonable and how far past it the mutant went. Both are answerable only if the
/// note carries the two numbers rather than merely the fact.
fn memory_note(binary: &camino::Utf8Path, peak: Option<u64>, limit: u64) -> String {
    let name = binary.file_name().unwrap_or(binary.as_str());
    let reached = peak.map_or_else(
        || "reached".to_owned(),
        |peak| format!("reached {}, past", crate::report::bytes(peak)),
    );

    format!(
        "`{name}` {reached} the {} this run allowed it",
        crate::report::bytes(limit)
    )
}

/// Describes a stall, given the last test the harness named.
///
/// The name is a landmark rather than a diagnosis, and the wording says so. libtest runs tests in
/// parallel and announces each one only once it has finished, so the test that is actually spinning
/// is by definition one it has not named. Wording that presents the name as the culprit sends
/// people to read a test that was fine, and — worse — invites a suppression on it.
fn stall_note(test: Option<&str>) -> String {
    test.map_or_else(
        || "stalled before the harness named a test".to_owned(),
        |name| format!("stalled, last test named was `{name}`"),
    )
}

/// One mutant's result: its index in the plan, what happened, how long it took and any detail.
type Completed = (usize, Outcome, u64, Option<String>, Option<String>);

/// Tests every live mutant in parallel, writing verdicts back onto the plan.
///
/// Workers publish each verdict over a channel that the calling thread drains while they are still
/// running, so the display moves as the run proceeds rather than jumping at the end.
///
/// # Errors
///
/// Returns an error if the memory accounting a run asked for stopped being installable partway
/// through. The sweep stops there: every later verdict would have been reached without the
/// protection the run was told it had, and a run cannot say which of its verdicts those were after
/// the fact.
fn test_all(
    work: &Workspace,
    plan: &mut Plan,
    binaries: &[TestBinary],
    sweep: Sweep,
    scope: TestScope<'_>,
    events: &mut impl Events,
) -> Result<()> {
    let Sweep { stall, jobs, meter } = sweep;

    let pending: Vec<usize> = plan
        .mutants
        .iter()
        .enumerate()
        .filter(|(_position, mutant)| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .map(|(position, _mutant)| position)
        .collect();

    if pending.is_empty() {
        return Ok(());
    }

    let next = AtomicUsize::new(0);
    let abandoned: OnceLock<String> = OnceLock::new();
    let ordinals: Vec<u32> = pending.iter().map(|position| plan.mutants[*position].ordinal).collect();

    // Reachability is resolved up front so that a worker needs nothing from the plan, leaving the
    // calling thread free to borrow it mutably and record verdicts as they arrive.
    let reachable: Vec<Vec<&TestBinary>> = pending
        .iter()
        .map(|position| {
            let package = &plan.mutants[*position].package;

            binaries.iter().filter(|binary| reaches(binary, package, plan, &scope)).collect()
        })
        .collect();

    let (sender, receiver) = mpsc::channel::<Completed>();

    thread::scope(|scope| {
        for _worker in 0..jobs.max(1) {
            let sender = sender.clone();
            let next = &next;
            let ordinals = &ordinals;
            let reachable = &reachable;
            let pending = &pending;
            let abandoned = &abandoned;

            let _handle = scope.spawn(move || {
                loop {
                    if abandoned.get().is_some() {
                        break;
                    }

                    let index = next.fetch_add(1, Ordering::Relaxed);

                    let Some(position) = pending.get(index).copied() else {
                        break;
                    };

                    let ordinal = ordinals[index];
                    let started = Instant::now();
                    let reachable = &reachable[index];

                    let judged = judge(work, ordinal, reachable, stall, meter);

                    let (outcome, killer, note) = match judged {
                        Judgement::Reached(outcome, killer, note) => (outcome, killer, note),
                        Judgement::Abandoned(reason) => {
                            let _first = abandoned.set(reason);

                            break;
                        }
                    };

                    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

                    // A closed receiver means the calling thread is gone, which cannot happen while
                    // the scope is open; there is nothing useful to do about it either way.
                    let _sent = sender.send((position, outcome, elapsed, killer, note));
                }
            });
        }

        // The workers hold the only remaining senders, so the drain ends when the last one finishes.
        drop(sender);

        for (position, outcome, elapsed, killer, note) in receiver {
            if let Some(mutant) = plan.mutants.get_mut(position) {
                mutant.outcome = outcome;
                mutant.elapsed_ms = elapsed;
                mutant.killed_by = killer;
                mutant.note = note;

                events.mutant(mutant);
            }
        }
    });

    abandoned.into_inner().map_or(Ok(()), |reason| {
        Err(error!(
            "the memory accounting this run asked for stopped being installable: {reason}.\n\
             The run stops here rather than judge the remaining mutants without the protection it \
             was asked for."
        ))
    })
}

/// The settings every mutant in a sweep is run under.
///
/// Carried together because they are decided once, before the first mutant, and read identically by
/// every worker; splitting them back out would only add arguments to the functions that thread them
/// through.
#[derive(Debug, Clone, Copy)]
struct Sweep {
    /// How long a test binary may go without saying anything before it is treated as stuck.
    stall: Stall,

    /// How many mutants to run at once.
    jobs: usize,

    /// Whether each run's memory is to be accounted for at all.
    meter: bool,
}

/// What one mutant's run across its reachable test binaries came to.
enum Judgement {
    /// The mutant was judged: an outcome, the test that caught it if one did, and any note.
    Reached(Outcome, Option<String>, Option<String>),

    /// The run could no longer be metered as asked, and no verdict from here on would mean anything.
    Abandoned(String),
}

/// Runs one mutant against every test binary that can reach it, stopping at the first detection.
///
/// Later binaries are not run once one has caught the mutant: the answer cannot change, and the
/// time saved is the difference between a sweep that finishes overnight and one that does not.
fn judge(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    stall: Stall,
    meter: bool,
) -> Judgement {
    // Nothing links this code, so no test can exercise it however it is mutated. Reporting that as
    // a survivor would blame the tests that exist for the absence of ones that do not.
    let survived = if reachable.is_empty() { Outcome::NoCoverage } else { Outcome::Survived };

    for binary in reachable {
        let request = Request { meter, limit: binary.memory };

        match run_binary(work, binary, Some(ordinal), binary.budget, stall, request) {
            Verdict::Passed => {}
            Verdict::Failed(name) => return Judgement::Reached(Outcome::Killed, name, None),
            Verdict::TimedOut => return Judgement::Reached(Outcome::Timeout, None, None),
            Verdict::Stalled(test) => {
                return Judgement::Reached(Outcome::Timeout, None, Some(stall_note(test.as_deref())));
            }

            // A detection, and recorded as one: the suite's own harness did not fail, but the
            // baseline established that this same workload fits under this same ceiling without the
            // mutant, so the mutant is what changed. It gets an outcome of its own so that a reader
            // is not sent looking for a failing assertion that never existed.
            Verdict::MemoryLimit { peak, limit } => {
                return Judgement::Reached(Outcome::OutOfMemory, None, Some(memory_note(&binary.path, peak, limit)));
            }
            Verdict::Unmetered(reason) => return Judgement::Abandoned(reason),
        }
    }

    Judgement::Reached(survived, None, None)
}

/// Describes what the baseline measured, for the line that replaces its announcement.
///
/// The test count is omitted rather than guessed when no harness announced one, which is what a
/// target built with `harness = false` does.
fn describe(baseline: &Baseline) -> String {
    let ran = baseline.tests.map_or_else(
        || format!("the suite passed in {:.1?}", baseline.elapsed),
        |tests| format!("{} ran in {:.1?}", crate::report::quantity(tests, "test"), baseline.elapsed),
    );

    // Reported whenever it was measured, whether or not anything is being enforced. A project
    // deciding whether a ceiling is worth turning on needs to know what its suite actually uses,
    // and this line is where that number is cheapest to notice.
    match baseline.peak {
        Some(peak) => format!("{ran}, peak {}", crate::report::bytes(peak)),
        None => ran,
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::ops::collect::Shape;

    fn sweep(stall: Stall) -> Sweep {
        Sweep { stall, jobs: 1, meter: false }
    }

    #[test]
    fn a_stall_does_not_claim_the_named_test_is_the_one_that_hung() {
        // Regression, issue-004. libtest names a test only once it has finished, so the test that
        // is spinning is precisely the one not named. Wording that presents the name as the culprit
        // sends people to read a test that was fine, and invites a suppression on it.
        let note = stall_note(Some("tests::round_trip"));

        assert!(note.contains("last test named was `tests::round_trip`"), "{note}");
        assert!(!note.contains("during"), "{note}");
        assert!(!note.contains(" in `"), "{note}");
    }

    #[test]
    fn a_stall_before_any_test_was_named_says_so() {
        let note = stall_note(None);

        assert_eq!(note, "stalled before the harness named a test");
    }

    /// A workspace, a plan holding one pending mutant, and a test binary that behaves as told.
    ///
    /// `test_all` is the scheduler, and the verdicts it has to translate into outcomes are exactly
    /// the ones a real suite produces least often, so the binary is a script rather than a
    /// compiled harness: the process machinery is real, only the suite is stand-in.
    #[cfg(unix)]
    fn harness(body: &str, budget: Duration) -> (tempfile::TempDir, Workspace, Plan, Vec<TestBinary>) {
        let (directory, work) = crate::testing::shell_workspace("test-all", body);
        let root = work.root.clone();

        let mutant = crate::model::Mutant {
            id: "m1".to_owned(),
            ordinal: 1,
            file: Utf8PathBuf::from("src/a.rs"),
            package: "subject".to_owned(),
            span: 0..1,
            line: 1,
            column: 1,
            mutator: "relational.gt_to_ge".to_owned(),
            item_path: "subject::f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a > b".to_owned(),
            replacement: "a >= b".to_owned(),
            shape: Shape::Expr,
            outcome: Outcome::Pending,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        };
        let plan = Plan {
            root,
            files: Vec::new(),
            mutants: vec![mutant],
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let binaries = vec![TestBinary {
            package: "subject".to_owned(),
            baseline: Duration::from_millis(1),
            budget,
            ..crate::testing::test_binary("/bin/sh")
        }];

        (directory, work, plan, binaries)
    }

    /// A mutant whose suite never finishes within its budget is recorded as a timeout.
    #[test]
    #[cfg(unix)]
    fn a_mutant_that_exhausts_its_budget_is_recorded_as_a_timeout() {
        let (_directory, work, mut plan, binaries) = harness("sleep 30", Duration::from_millis(50));
        let scope = TestScope { packages: &[], whole_workspace: true };

        test_all(&work, &mut plan, &binaries, sweep(Stall::NONE), scope, &mut crate::testing::Recorder::default())
            .expect("the sweep completes");

        // A hang counts as a detection: the suite noticed the mutant, even if it noticed by
        // never coming back rather than by failing an assertion.
        assert_eq!(plan.mutants[0].outcome, Outcome::Timeout);
        assert_eq!(plan.mutants[0].note, None);
    }

    /// A mutant whose suite goes silent is a timeout, annotated with where it went silent.
    #[test]
    #[cfg(unix)]
    fn a_mutant_that_stalls_is_recorded_as_a_timeout_naming_the_test() {
        let (_directory, work, mut plan, binaries) = harness("echo 'test slow::case ... '\nsleep 30", Duration::from_secs(60));
        let scope = TestScope { packages: &[], whole_workspace: true };
        let stall = Stall { budget: Some(Duration::from_millis(50)) };

        test_all(&work, &mut plan, &binaries, sweep(stall), scope, &mut crate::testing::Recorder::default())
            .expect("the sweep completes");

        // Saying which test was running when the silence started is the whole value of stall
        // detection over simply waiting out the budget.
        assert_eq!(plan.mutants[0].outcome, Outcome::Timeout);
        assert!(plan.mutants[0].note.is_some(), "{:?}", plan.mutants[0].note);
    }

    /// A mutant no test binary can reach is uncovered rather than a survivor.
    #[test]
    #[cfg(unix)]
    fn a_mutant_no_binary_reaches_is_uncovered() {
        let (_directory, work, mut plan, binaries) = harness("exit 0", Duration::from_secs(30));
        let scope = TestScope { packages: &["other".to_owned()], whole_workspace: false };

        test_all(&work, &mut plan, &binaries, sweep(Stall::NONE), scope, &mut crate::testing::Recorder::default())
            .expect("the sweep completes");

        // Blaming the tests that exist for code nothing links would make the score a measure of
        // the build graph rather than of the suite.
        assert_eq!(plan.mutants[0].outcome, Outcome::NoCoverage);
    }

    #[test]
    fn a_baseline_without_a_harness_count_is_described_by_elapsed_time_only() {
        let baseline = Baseline {
            elapsed: Duration::from_millis(1500),
            quiet: Duration::ZERO,
            tests: None,
            peak: None,
        };

        // Custom test harnesses may never announce a count; reporting the elapsed fixed cost is
        // still useful, but inventing a count would be misleading.
        assert_eq!(describe(&baseline), "the suite passed in 1.5s");
    }
}
