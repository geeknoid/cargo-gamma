use camino::Utf8Path;
use core::num::NonZero;
use core::time::Duration;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::error::error;
use crate::discover::Plan;
use crate::model::Mutant;
use crate::report::{Listings, Progress, Styler, quantity};

use super::cli::RunArgs;
use super::console_events::ConsoleEvents;
use super::dispatch::{DEFAULT_TIMEOUT_MULTIPLIER, EXIT_GATE_FAILED, EXIT_OK};
use super::host::Host;
use super::when::When;

/// Which of the bulk outcome listings the caller asked for.
const fn listings(args: &RunArgs, announced: bool) -> Listings {
    Listings {
        caught: args.caught,
        unviable: args.unviable,
        announced,
    }
}

/// Loads `.cargo/gamma.toml` and folds it into `args`.
pub(super) fn configure<H: Host>(host: &mut H, args: &mut RunArgs, styler: Styler) -> crate::Result<()> {
    // Said before anything is loaded, because the settings in that file are about to not happen and
    // the run would otherwise look like it honoured them.
    if Config::foreign_present(&args.select.dir) && !args.select.config.no_config && args.select.config.path.is_none() {
        let note = styler.verb("Note");

        writeln!(
            host.error(),
            "{note} .cargo/mutants.toml is not read; run `cargo gamma migrate` to translate it"
        )?;
    }

    Config::resolve(&args.select)?.apply(args);

    Ok(())
}

/// Works out how much memory control the run should place around each test binary.
///
/// The two size flags imply a mode, because asking for a specific ceiling and then being told
/// nothing was enforced would be a surprising way to learn that a separate switch existed. Naming
/// `--memory` explicitly still wins, so a configuration file that turns metering on can be turned
/// back off for one run.
///
/// Whether any of that was said out loud is recorded rather than discarded. Enforcement is the
/// default, and a host that cannot deliver it gets a note and an unbounded run — but a user who
/// named a memory setting gets an error instead, because they asked for a guarantee and silently
/// not having it is the one outcome that could cost them the machine.
fn memory_policy(args: &RunArgs) -> crate::exec::MemoryPolicy {
    let measure = args.measure.baseline_memory_limit.is_some();
    let enforce = args.measure.memory_limit.is_some();

    let implied = if enforce {
        Some(crate::exec::MemoryControl::Enforce)
    } else if measure {
        Some(crate::exec::MemoryControl::Measure)
    } else {
        None
    };

    let stated = args.measure.memory.or(implied);
    let demand = if stated.is_some() {
        crate::exec::Demand::Stated
    } else {
        crate::exec::Demand::Inherited
    };

    crate::exec::MemoryPolicy {
        control: stated.unwrap_or_default(),
        demand,
        multiplier: args.measure.memory_multiplier.unwrap_or(crate::exec::DEFAULT_MULTIPLIER),
        headroom: args.measure.memory_headroom.unwrap_or(crate::exec::DEFAULT_HEADROOM),
        limit: args.measure.memory_limit,
        baseline_limit: args.measure.baseline_memory_limit,
    }
}

/// Records what `merge` needs to know about this run.
///
/// The shard identity travels in the report rather than in the filename, because a filename is a
/// convention and this has to survive being copied into an artifact bucket by someone who does not
/// know the convention.
fn run_info(args: &RunArgs) -> crate::elements::RunInfo {
    crate::elements::RunInfo {
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        shard: args
            .select
            .shard_count
            .zip(args.select.shard_index)
            .map(|(count, index)| crate::elements::ShardInfo { index, count }),
    }
}

/// Writes whichever file reports were asked for, and says where they went.
///
/// The path is echoed because a report written to a path nobody looks at is the same as no report,
/// and in CI the message is often the only trace that the artifact exists.
fn emit_reports<H: Host>(host: &mut H, args: &RunArgs, plan: &Plan, advice: Option<&str>, styler: Styler) -> crate::Result<()> {
    emit_ci(host, args, plan, advice, styler)?;

    if args.html.is_none() && args.json_report.is_none() {
        return Ok(());
    }

    let report = crate::elements::build(plan, crate::elements::Thresholds::default(), Some(run_info(args)))?;
    let mut stream = host.error();

    if let Some(path) = args.json_report.as_ref() {
        crate::elements::write(path, &crate::elements::to_json(&report)?)?;
        writeln!(stream, "{} {path}", styler.verb("Wrote"))?;
    }

    if let Some(path) = args.html.as_ref() {
        let source = if args.html_external {
            crate::html::Source::External
        } else {
            crate::html::Source::Inline
        };

        crate::elements::write(path, &crate::html::render(&report, source)?)?;
        writeln!(stream, "{} {path}", styler.verb("Wrote"))?;
    }

    Ok(())
}

/// Writes the SARIF log, the diff annotations and the job summary.
///
/// All three publish survivors and nothing else, so a run that caught everything is silent here.
/// That silence is the point: a CI surface that speaks every night regardless of what happened is
/// one people learn to scroll past.
fn emit_ci<H: Host>(host: &mut H, args: &RunArgs, plan: &Plan, advice: Option<&str>, styler: Styler) -> crate::Result<()> {
    if let Some(path) = args.sarif.as_ref() {
        let (log, truncation) = crate::ci::sarif(&plan.mutants, &plan.root, args.sarif_level)?;

        crate::elements::write(path, &log)?;

        let mut stream = host.error();

        writeln!(stream, "{} {path}", styler.verb("Wrote"))?;

        if let Some(truncation) = truncation {
            // Saying so is the whole difference between a report that is smaller than the truth and
            // a report that is quietly wrong.
            writeln!(
                stream,
                "{} {} of {} findings written; SARIF consumers reject a larger log outright",
                styler.verb("Note"),
                truncation.written,
                truncation.found
            )?;
        }
    }

    if !crate::ci::wanted(args.annotations, host.env("GITHUB_ACTIONS").is_some()) {
        return Ok(());
    }

    for line in crate::ci::annotations(&plan.mutants, &plan.root) {
        writeln!(host.output(), "{line}")?;
    }

    if let Some(path) = host.env("GITHUB_STEP_SUMMARY") {
        // The diagnosis rides along with the score rather than in a file of its own: the summary
        // panel is the artifact a team reads every morning, and a score nobody knows what to do
        // about is the reason mutation testing gets run nightly and then ignored.
        let mut summary = crate::ci::summary(&plan.mutants, &plan.root);

        if let Some(advice) = advice {
            summary.push('\n');
            summary.push_str(advice);
        }

        OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .and_then(|mut file| file.write_all(summary.as_bytes()))
            .map_err(|cause| crate::error::error!("could not write the job summary to {path}").caused_by(cause))?;
    }

    Ok(())
}

/// Collects the mutants whose `expect_missed` or `expect_caught` directive did not hold.
///
/// An expectation is a claim about the suite that the author asked to be held to. Parsing it and
/// then ignoring it is worse than not supporting it at all: the directive reads as a guarantee, and
/// the thing it guards can rot indefinitely without anyone hearing about it.
///
/// Only mutants that actually ran are judged. One that failed to compile or was never reached is
/// not evidence either way, and failing a run over it would make the check depend on whether the
/// build happened to produce that mutant at all.
fn broken_expectations(mutants: &[Mutant]) -> Vec<(&Mutant, &'static str)> {
    let mut broken = Vec::new();

    for mutant in mutants {
        let Some(expectation) = &mutant.expectation else {
            continue;
        };

        if !mutant.outcome.is_valid() {
            continue;
        }

        let detected = mutant.outcome.is_detected();

        if detected != expectation.caught {
            broken.push((mutant, if expectation.caught { "caught" } else { "missed" }));
        }
    }

    broken
}

pub(super) fn run_session<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<i32> {
    let Some(plan) = execute(host, args, progress_when, styler)? else {
        return Ok(EXIT_OK);
    };

    let summary = crate::model::Summary::of(&plan.mutants);
    let broken = broken_expectations(&plan.mutants);

    if !broken.is_empty() {
        let mut stream = host.error();

        for (mutant, wanted) in &broken {
            let line = mutant.expectation.as_ref().map_or(mutant.line, |expectation| expectation.line);
            let was = mutant.outcome;
            let _ = writeln!(
                stream,
                "{} {}:{line}: `{}` expected this mutant to be {wanted}, but it was {was}",
                styler.error("error:"),
                mutant.file,
                mutant.mutator
            );
        }

        let count = broken.len();
        let _ = writeln!(
            stream,
            "{} {count} {} not hold",
            styler.error("error:"),
            if count == 1 { "expectation did" } else { "expectations did" }
        );

        return Ok(EXIT_GATE_FAILED);
    }

    if let Some(minimum) = args.min_score
        && summary.score() < minimum
    {
        let mut stream = host.error();
        let _ = writeln!(
            stream,
            "{} mutation score {:.1}% is below the required {minimum:.1}%",
            styler.error("error:"),
            summary.score()
        );

        return Ok(EXIT_GATE_FAILED);
    }

    Ok(EXIT_OK)
}

/// Collects the arguments every test binary should receive.
///
/// `--cargo-test-arg` and everything after `--` mean the same thing to the harness, so they are
/// concatenated in the order they were written rather than kept apart.
fn test_arguments(args: &RunArgs) -> Vec<String> {
    let mut collected = args.measure.cargo_test_args.clone();

    collected.extend(args.measure.test_args.iter().cloned());
    collected
}

/// Reads the mutants an earlier report already settled.
///
/// Only survivors and mutants that were never reached are worth retrying: a mutant the suite
/// caught stays caught unless the suite changed, and one that could not compile will not compile
/// now either. Anything the report does not mention is new, and new mutants are exactly what an
/// iterative run exists to reach.
fn settled_mutants(report: &Utf8Path) -> crate::Result<crate::HashSet<String>> {
    let text = fs::read_to_string(report)
        .map_err(|cause| error!("could not read the earlier report `{report}`").caused_by(cause))?;

    crate::elements::settled_mutants(&text)
        .map_err(|cause| error!("`{report}` is not a report this tool wrote: {cause}").usage())
}

/// Says how many mutants an earlier report spared this run.
fn report_iteration<H: Host>(host: &mut H, args: &RunArgs, plan: &Plan, styler: Styler) -> crate::Result<()> {
    let Some(report) = args.iterate.as_ref() else {
        return Ok(());
    };

    if plan.settled_out == 0 {
        return Ok(());
    }

    writeln!(
        host.error(),
        "{} {} already settled by {report}",
        styler.verb("Iterating"),
        quantity(plan.settled_out, "mutant")
    )?;

    Ok(())
}

/// Discovers, runs and reports, returning the completed plan.
///
/// Returns `None` when there was nothing to run, which is not a failure but leaves nothing for a
/// caller to inspect. Split out of [`run_session`] so `suppress` can act on the verdicts rather than
/// re-deriving them from a second run.
pub(super) fn execute<H: Host>(
    host: &mut H,
    args: &RunArgs,
    progress_when: When,
    styler: Styler,
) -> crate::Result<Option<Plan>> {
    let started = Instant::now();
    let selection = args.select.selection()?;
    let shard = args.select.shard()?;
    let visible = progress_when.resolve(host.is_terminal());
    let mut progress = Progress::new(visible, styler, host.terminal_width());
    let mut survey = crate::discover::Survey::new(&args.select, shard)?;

    if let Some(report) = args.iterate.as_ref() {
        survey.settle(settled_mutants(report)?);
    }

    // A dry run reports on the whole population and builds nothing, so there is no package-by-
    // package sequence to interleave the scan with; it is simply scanned.
    if args.dry_run {
        let mut ordinals = 0;
        let scanned = survey.scan(None, &selection, &mut ordinals)?;
        let plan = survey.into_plan(scanned);

        progress.finish(host);
        report_iteration(host, args, &plan, styler)?;

        if plan.mutants.is_empty() {
            let _ = writeln!(host.error(), "no mutants were generated");

            return Ok(None);
        }

        crate::report::summarize(host, &plan, styler, listings(args, false))?;
        emit_reports(host, args, &plan, None, styler)?;
        emit_diag(host, args, &plan, None, started)?;

        return Ok(Some(plan));
    }

    let config = crate::exec::Config {
        jobs: args.measure.jobs.unwrap_or_else(|| {
            thread::available_parallelism().map_or(1, NonZero::get)
        }),
        timeout_multiplier: args.measure.timeout_multiplier.unwrap_or(DEFAULT_TIMEOUT_MULTIPLIER),
        timeout: args.measure.timeout.map(Duration::from_secs_f64),
        baseline: !args.no_baseline,
        stall: !args.no_stall_detection,
        cargo: crate::exec::CargoOptions {
            features: args.select.features.to_cargo_args(),
            profile: args.measure.profile.clone(),
            extra: args.measure.cargo_args.clone(),
            test_args: test_arguments(args),
        },
        memory: memory_policy(args),
        build: crate::exec::BuildLimits {
            timeout: args.limits.build_timeout.map(Duration::from_secs_f64),
            multiplier: args.limits.build_timeout_multiplier,
            rollback_rounds: args.limits.rollback_rounds,
        },
        leak_dirs: args.leak_dirs,
        scratch_dir: args.measure.scratch_dir.clone(),
        test_packages: args.measure.test_packages.clone(),
        include_tests: args.measure.include_tests.clone(),
        exclude_tests: args.measure.exclude_tests.clone(),
        test_workspace: args.measure.test_workspace,
        ..crate::exec::Config::default()
    };

    let config = crate::exec::Config {
        timeout_floor: args
            .measure
            .minimum_test_timeout
            .map_or(config.timeout_floor, Duration::from_secs_f64),
        ..config
    };

    let mut events = ConsoleEvents {
        host,
        progress,
        styler,
        estimate: args.estimate,
    };

    let outcome = crate::exec::run(&survey, &selection, &config, &mut events);

    // A phase that failed never got to say what it found, so the line it opened is still waiting
    // for an ending. Close it before the error is printed, or the error arrives as the rest of
    // that sentence.
    if outcome.is_err() {
        events.abandon();
    }

    let crate::exec::Measured { plan, built } = outcome?;

    let mut progress = events.progress;

    // The live display named every survivor and timeout as it happened, so the summary must not
    // name them again.
    let announced = progress.is_enabled();

    progress.finish(host);
    report_iteration(host, args, &plan, styler)?;

    if plan.mutants.is_empty() {
        let _ = writeln!(host.error(), "no mutants were generated");

        return Ok(None);
    }

    // With nothing live there was no build to pay for; the summary already accounts for every
    // mutant that was suppressed, sharded away or already settled.
    let Some(built) = built else {
        crate::report::summarize(host, &plan, styler, listings(args, announced))?;

        emit_reports(host, args, &plan, None, styler)?;
        emit_diag(host, args, &plan, None, started)?;

        return Ok(Some(plan));
    };

    if args.leak_dirs {
        let tree = crate::exec::scratch_tree(&plan.root, args.measure.scratch_dir.as_deref());

        writeln!(host.error(), "{} {tree}", styler.verb("Kept"))?;
    }

    crate::report::summarize(host, &plan, styler, listings(args, announced))?;
    crate::report::session_notes(host, &built.session, styler)?;

    // The job summary wants a fragment under the heading it already owns; `--advice` wants a
    // whole document. Same analysis, two shapes.
    let panel = advice_markdown(args, &plan, &built.session, crate::advise::Layout::Embedded);

    emit_reports(host, args, &plan, Some(&panel), styler)?;
    emit_advice(host, args, &plan, &built.session, styler)?;
    emit_diag(host, args, &plan, Some(&built.session), started)?;

    Ok(Some(plan))
}

/// Dumps the run's own numbers, when the hidden `--diag` asked for them.
///
/// Last, and to the diagnostic stream, because it is neither a result nor something a person
/// reading the summary asked to see.
fn emit_diag<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    session: Option<&crate::exec::Session>,
    started: Instant,
) -> crate::Result<()> {
    if !args.diag {
        return Ok(());
    }

    let jobs = args.measure.jobs.unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZero::get));

    write!(host.error(), "\n{}", crate::diag::render(plan, session, jobs, started.elapsed()))?;

    Ok(())
}

/// Writes the Markdown diagnosis, when `--advice` asked for one.
fn emit_advice<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    session: &crate::exec::Session,
    styler: Styler,
) -> crate::Result<()> {
    let Some(path) = args.advice.as_ref() else {
        return Ok(());
    };

    let advice = advice_markdown(args, plan, session, crate::advise::Layout::Document);

    fs::write(path, &advice)
        .map_err(|cause| crate::error::error!("could not write the advice to {path}").caused_by(cause))?;

    writeln!(host.error(), "{} {path}", styler.verb("Wrote"))?;

    Ok(())
}

/// Renders the diagnosis and the family table as Markdown.
///
/// The family table is part of the diagnosis rather than a separate feature: knowing that a run's
/// time went somewhere is only actionable alongside what that somewhere caught.
fn advice_markdown(args: &RunArgs, plan: &Plan, session: &crate::exec::Session, layout: crate::advise::Layout) -> String {
    let timing = crate::advise::Timing {
        build: session.build,
        baseline: session.baseline,
        wall: session.build + session.baseline + total_elapsed(&plan.mutants, args.measure.jobs),
        jobs: args.measure.jobs.unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZero::get)),
    };

    let findings = crate::advise::analyze(&plan.mutants, &timing);

    let summary = crate::model::Summary::of(&plan.mutants);

    crate::advise::render_markdown(&findings, &crate::advise::yields(&plan.mutants), summary, &timing, layout)
}

/// Wall time spent testing mutants, from the CPU time they used and the parallelism they used it at.
fn total_elapsed(mutants: &[Mutant], jobs: Option<usize>) -> Duration {
    let cpu: Duration = mutants.iter().map(|mutant| Duration::from_millis(mutant.elapsed_ms)).sum();
    let jobs = jobs.unwrap_or_else(|| thread::available_parallelism().map_or(1, NonZero::get));

    cpu / u32::try_from(jobs.max(1)).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use crate::discover::{Plan, TargetFile};
    use crate::exec::Session;
    use crate::model::{Mutant, Outcome};
    use crate::ops::collect::Shape;

    use super::*;
    use crate::testing::{Sink, fails_at_every_line, workdir};

    fn plan() -> Plan {
        let dir = workdir("run-plan-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.keep()).expect("utf8");
        let src = root.join("src");
        fs::create_dir(&src).expect("src");
        let source = "pub fn less(a: i32, b: i32) -> bool { a < b }\n";
        let absolute = src.join("lib.rs");
        fs::write(&absolute, source).expect("source");
        let start = source.find("a < b").expect("span");

        Plan {
            root,
            files: vec![TargetFile {
                path: camino::Utf8PathBuf::from("src/lib.rs"),
                absolute,
                package: "subject".to_owned(),
            }],
            mutants: vec![
                mutant(start, Outcome::Survived, 120),
                mutant(start, Outcome::NoCoverage, 80),
            ],
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    fn mutant(start: usize, outcome: Outcome, elapsed_ms: u64) -> Mutant {
        Mutant {
            id: format!("m{elapsed_ms}"),
            ordinal: 1,
            file: camino::Utf8PathBuf::from("src/lib.rs"),
            package: "subject".to_owned(),
            span: start..start + 5,
            line: 1,
            column: start + 1,
            mutator: "relational.lt_to_le".to_owned(),
            item_path: "subject::less".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a < b".to_owned(),
            replacement: "a <= b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms,
            killed_by: None,
            note: None,
        }
    }

    fn expecting(outcome: Outcome, caught: bool) -> Mutant {
        Mutant {
            expectation: Some(crate::model::Expectation { caught, line: 3, reason: None }),
            ..mutant(0, outcome, 1)
        }
    }

    #[test]
    fn an_expectation_that_holds_is_not_reported() {
        let mutants = vec![
            expecting(Outcome::Killed, true),
            expecting(Outcome::Survived, false),
            expecting(Outcome::Timeout, true),
        ];

        assert!(broken_expectations(&mutants).is_empty());
    }

    #[test]
    fn an_expectation_that_does_not_hold_is_reported_with_what_was_wanted() {
        let mutants = vec![expecting(Outcome::Survived, true), expecting(Outcome::Killed, false)];
        let broken = broken_expectations(&mutants);

        assert_eq!(broken.len(), 2);
        assert_eq!(broken[0].1, "caught");
        assert_eq!(broken[1].1, "missed");
    }

    #[test]
    fn an_expectation_on_a_mutant_that_never_ran_is_not_judged() {
        // A mutant that failed to compile or that nothing reaches is not evidence about the suite
        // either way, so holding the author to a claim about it would fail runs for no reason.
        let mutants = vec![
            expecting(Outcome::CompileError, true),
            expecting(Outcome::Ignored, true),
            expecting(Outcome::Pending, true),
        ];

        assert!(broken_expectations(&mutants).is_empty());
    }

    #[test]
    fn an_uncovered_mutant_is_judged_against_its_expectation() {
        // Nothing reaching a site is exactly what `expect_missed` claims, and the opposite of what
        // `expect_caught` claims, so both are real answers.
        assert!(broken_expectations(&[expecting(Outcome::NoCoverage, false)]).is_empty());
        assert_eq!(broken_expectations(&[expecting(Outcome::NoCoverage, true)]).len(), 1);
    }

    #[test]
    fn a_mutant_with_no_expectation_is_never_reported() {
        assert!(broken_expectations(&[mutant(0, Outcome::Survived, 1)]).is_empty());
    }

    fn session() -> Session {        Session {
            baseline: Duration::from_millis(20),
            quiet: Duration::from_millis(10),
            stall: Some(Duration::from_millis(30)),
            timeout: Duration::from_millis(50),
            build: Duration::from_millis(40),
            metered: false,
            unbounded: None,
            withdrawn: 0,
            rounds: 1,
            binaries: Vec::new(),
            peak: None,
            footprint: 0,
            filtered: 0,
            not_built: 0,
            widened: false,
        }
    }

    /// Every line the report writer emits is checked to propagate a closed stream.
    #[test]
    fn a_closed_stream_stops_the_report_writer_at_whichever_line_it_reached() {
        let dir = workdir("run-reports-closed-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let plan = plan();

        // A run that has written a SARIF log, a JSON report and an HTML report emits several
        // lines; a pipe that closes partway through must fail the command rather than be ignored,
        // or CI would record a success for reports nobody received.
        fails_at_every_line(3, |host| {
            let args = RunArgs {
                json_report: Some(root.join("result.json")),
                html: Some(root.join("result.html")),
                html_external: true,
                sarif: Some(root.join("result.sarif")),
                annotations: crate::ci::Annotations::None,
                ..Default::default()
            };

            emit_reports(host, &args, &plan, None, Styler::new(false))
        });
    }

    /// Annotations go to the results stream, and a closed one stops the run.
    #[test]
    fn a_closed_results_stream_stops_the_annotation_writer() {
        let plan = plan();

        // The plan holds one survivor, so exactly one annotation line is published.
        fails_at_every_line(1, |host| {
            let args = RunArgs {
                annotations: crate::ci::Annotations::Github,
                ..Default::default()
            };

            emit_ci(host, &args, &plan, None, Styler::new(false))
        });
    }

    #[test]
    fn reports_write_json_html_sarif_annotations_and_summary() {
        let dir = workdir("run-reports-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let summary = root.join("summary.md");
        let args = RunArgs {
            json_report: Some(root.join("reports/result.json")),
            html: Some(root.join("reports/result.html")),
            html_external: true,
            sarif: Some(root.join("reports/result.sarif")),
            annotations: crate::ci::Annotations::Github,
            ..Default::default()
        };

        let mut host = Sink::default().with_env("GITHUB_STEP_SUMMARY", summary.as_str());
        let plan = plan();

        emit_reports(&mut host, &args, &plan, Some("embedded advice"), Styler::new(false)).expect("reports");

        let out = String::from_utf8(host.out).expect("utf-8");
        let err = String::from_utf8(host.err).expect("utf-8");
        let summary_text = fs::read_to_string(summary).expect("summary");

        assert!(args.json_report.as_ref().expect("json path").exists());
        assert!(args.html.as_ref().expect("html path").exists());
        assert!(args.sarif.as_ref().expect("sarif path").exists());
        assert!(out.contains("::warning"), "{out}");
        assert!(err.contains("Wrote"), "{err}");
        assert!(summary_text.contains("embedded advice"), "{summary_text}");
    }

    #[test]
    fn advice_and_diag_are_emitted_only_when_requested() {
        let dir = workdir("run-advice-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);
        args.advice = Some(root.join("advice.md"));
        args.diag = true;

        let plan = plan();
        let session = session();
        let mut host = Sink::default();

        emit_advice(&mut host, &args, &plan, &session, Styler::new(false)).expect("advice");
        emit_diag(&mut host, &args, &plan, Some(&session), Instant::now()).expect("diag");

        let advice = fs::read_to_string(args.advice.as_ref().unwrap()).expect("advice file");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(advice.contains("Mutation testing"), "{advice}");
        assert!(err.contains("Wrote"), "{err}");
        assert!(err.contains("diag"), "{err}");
    }

    /// A leftover cargo-mutants config is called out, because the run does not honour it.
    #[test]
    fn a_foreign_config_is_called_out_before_anything_is_loaded() {
        let dir = workdir("run-foreign-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "examine_globs = [\"src/**\"]\n").expect("foreign");

        let mut args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                ..crate::commands::SelectArgs::default()
            },
            ..Default::default()
        };
        let mut host = Sink::default();

        configure(&mut host, &mut args, Styler::new(false)).expect("configure");

        assert!(host.err().contains("is not read"), "{}", host.err());
        assert!(host.err().contains("cargo gamma migrate"), "{}", host.err());
    }

    /// Asking not to read config suppresses the note along with the loading.
    #[test]
    fn a_foreign_config_is_not_mentioned_when_config_is_disabled() {
        let dir = workdir("run-foreign-off-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "examine_globs = [\"src/**\"]\n").expect("foreign");

        let mut select = crate::commands::SelectArgs {
            dir: root,
            ..crate::commands::SelectArgs::default()
        };

        select.config.no_config = true;

        let mut args = RunArgs { select, ..Default::default() };
        let mut host = Sink::default();

        configure(&mut host, &mut args, Styler::new(false)).expect("configure");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    /// An `--iterate` report that is not there names itself rather than failing anonymously.
    #[test]
    fn an_unreadable_iterate_report_names_itself() {
        let dir = workdir("run-iterate-missing-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let missing = root.join("gone.json");

        let error = settled_mutants(&missing).expect_err("missing report");

        assert!(error.to_string().contains(missing.as_str()), "{error}");
    }

    /// A file that is not one of our reports is the user's mistake, not an internal failure.
    #[test]
    fn an_iterate_report_that_is_not_ours_is_a_usage_error() {
        let dir = workdir("run-iterate-foreign-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("other.json");
        fs::write(&path, "{\"unrelated\": true}").expect("write");

        let error = settled_mutants(&path).expect_err("foreign report");

        assert!(error.is_usage(), "{error}");
    }

    /// An iterative run says how much of the population the earlier report spared it.
    #[test]
    fn an_iterative_run_says_how_many_mutants_were_already_settled() {
        let args = RunArgs {
            iterate: Some(camino::Utf8PathBuf::from("previous.json")),
            ..Default::default()
        };
        let mut plan = plan();

        plan.settled_out = 12;

        let mut host = Sink::default();

        report_iteration(&mut host, &args, &plan, Styler::new(false)).expect("iteration");

        assert!(host.err().contains("Iterating"), "{}", host.err());
        assert!(host.err().contains("12 mutants"), "{}", host.err());
    }

    /// Nothing is said when there is no earlier report, or when it spared nothing.
    #[test]
    fn a_run_that_settled_nothing_says_nothing_about_iterating() {
        let plan = plan();
        let mut host = Sink::default();

        report_iteration(&mut host, &RunArgs::default(), &plan, Styler::new(false)).expect("no iterate");

        let args = RunArgs {
            iterate: Some(camino::Utf8PathBuf::from("previous.json")),
            ..Default::default()
        };

        report_iteration(&mut host, &args, &plan, Styler::new(false)).expect("nothing settled");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    /// A closed stream has to surface from the iteration note.
    #[test]
    fn a_closed_stream_is_reported_by_the_iteration_note() {
        let args = RunArgs {
            iterate: Some(camino::Utf8PathBuf::from("previous.json")),
            ..Default::default()
        };
        let mut plan = plan();

        plan.settled_out = 3;

        fails_at_every_line(1, |host| report_iteration(host, &args, &plan, Styler::new(false)));
    }

    /// A survivor count past the SARIF ceiling is said out loud rather than silently trimmed.
    #[test]
    fn a_truncated_sarif_log_says_how_much_it_left_out() {
        let dir = workdir("run-sarif-truncated-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = RunArgs {
            sarif: Some(root.join("result.sarif")),
            ..Default::default()
        };

        let mut plan = plan();
        let survivor = plan.mutants[0].clone();

        plan.mutants = (0..=crate::ci::SARIF_LIMIT)
            .map(|index| {
                let mut mutant = survivor.clone();

                mutant.id = format!("m{index}");
                mutant
            })
            .collect();

        let mut host = Sink::default();

        emit_ci(&mut host, &args, &plan, None, Styler::new(false)).expect("ci");

        assert!(host.err().contains("findings written"), "{}", host.err());
        assert!(host.err().contains("reject a larger log outright"), "{}", host.err());
    }

    /// A dry run over an earlier report skips what that report already settled.
    #[test]
    fn a_dry_run_with_an_earlier_report_settles_what_it_already_knows() {
        let dir = workdir("run-iterate-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir(root.join("src")).expect("src");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("manifest");
        fs::write(root.join("src/lib.rs"), "pub fn less(a: i32, b: i32) -> bool { a < b }\n").expect("lib");

        let mut args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root.clone(),
                ..crate::commands::SelectArgs::default()
            },
            dry_run: true,
            ..Default::default()
        };

        // A first pass names the population, which the second pass then declares already settled.
        let mut host = Sink::default();
        let first = execute(&mut host, &args, When::Never, Styler::new(false))
            .expect("first pass")
            .expect("a population");

        assert!(!first.mutants.is_empty());

        // Recording every mutant as caught is what makes the second pass find nothing left to do.
        let mut settled = first;

        for mutant in &mut settled.mutants {
            mutant.outcome = Outcome::Killed;
        }

        let report = root.join("previous.json");
        let built = crate::elements::build(&settled, crate::elements::Thresholds::default(), None).expect("report");
        fs::write(&report, crate::elements::to_json(&built).expect("json")).expect("write");

        args.iterate = Some(report);

        let mut host = Sink::default();
        let second = execute(&mut host, &args, When::Never, Styler::new(false)).expect("second pass");

        assert!(second.is_none(), "everything was already settled");
        assert!(host.err().contains("no mutants were generated"), "{}", host.err());
        assert!(host.err().contains("already settled"), "{}", host.err());
    }

    /// And from the foreign-config note.
    #[test]
    fn a_closed_stream_is_reported_by_the_foreign_config_note() {
        let dir = workdir("run-foreign-broken-");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "examine_globs = [\"src/**\"]\n").expect("foreign");

        // `configure` folds the file into `args`, so each attempt needs its own copy of them.
        fails_at_every_line(1, |host| {
            let mut args = RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                ..Default::default()
            };

            configure(host, &mut args, Styler::new(false))
        });
    }
}
