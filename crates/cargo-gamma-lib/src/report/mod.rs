//! Projections of a run into things a person or a program can read.
//!
//! The console rendering follows `cargo build`: a right-aligned twelve-column bold-green verb, the
//! subject after it, and a single progress line that is rewritten in place. Matching cargo is not
//! decoration. A developer already reads cargo output fluently, and a tool that puts its status in
//! the same shape is one they do not have to learn.

mod progress;
mod styler;

use std::io::Write as _;

use crate::Result;
use crate::commands::Host;
use crate::discover::Plan;
use crate::exec::Session;
use crate::model::{Mutant, Outcome};

pub use progress::Progress;
pub use styler::Styler;

/// Width of the status verb column, matching cargo.
pub(crate) const VERB_WIDTH: usize = 12;

/// The empty status column, for a line that continues the one above it.
#[must_use]
pub fn continuation() -> String {
    " ".repeat(VERB_WIDTH)
}

/// Renders a count with its noun, pluralized.
///
/// Only regular nouns are ever counted here, so the rule is the naive one.
#[must_use]
pub fn quantity(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Which of the uninteresting outcomes the reader asked to see listed in full.
///
/// Survivors are always listed, because they are the result. Everything else is bulk that a healthy
/// run produces thousands of, and printing it by default buries the finding it surrounds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Listings {
    /// List every mutant the suite caught.
    pub caught: bool,

    /// List every mutant that could not be compiled.
    pub unviable: bool,

    /// Whether the live display already named the survivors and timeouts as they happened.
    ///
    /// When it did, repeating them here prints every finding twice on one screen and buries the
    /// `Found` and `Summary` lines between the two copies. When it did not — output is piped, or
    /// progress is off — the listing is the only place the results appear, and it is not optional:
    /// stdout carries the results.
    pub announced: bool,
}

/// Names the mutants that took no part in the score, as a tail for the summary line.
///
/// Each of these changes what the total is a total of, so leaving them out entirely would make the
/// score look like it covered more than it did. None of them is a finding about the code, so each
/// appears only when it is not zero, and a clean run gets nothing.
///
/// Unviable mutants are the exception and are never named here. They are a fact about what the
/// compiler would accept rather than about what the tests check, they are withdrawn automatically,
/// and on a large workspace there are thousands of them — a number nobody acts on, sitting on the
/// one line everybody reads. `-V` lists them and `--diag` counts them.
fn excluded(plan: &Plan) -> String {
    let mut parts: Vec<String> = Vec::new();

    if plan.suppressed > 0 {
        parts.push(format!("{} suppressed", plan.suppressed));
    }

    let not_built = plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::NotBuilt).count();

    if not_built > 0 {
        parts.push(format!("{not_built} not built"));
    }

    if plan.sharded_out > 0 {
        parts.push(format!("{} outside this shard", plan.sharded_out));
    }

    if plan.settled_out > 0 {
        parts.push(format!("{} already settled", plan.settled_out));
    }

    if parts.is_empty() {
        return String::new();
    }

    format!(", {}", parts.join(", "))
}

/// Writes the end-of-run summary.
pub fn summarize<H: Host>(host: &mut H, plan: &Plan, styler: Styler, listings: Listings) -> Result<()> {
    let summary = crate::model::Summary::of(&plan.mutants);
    let heading = styler.verb("Summary");
    let survivors: Vec<&Mutant> =
        plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Survived).collect();

    // Gathered before anything is written so that the blank lines between them can be placed
    // without the writing having to know what comes next: every block is preceded by one, and one
    // more closes the last of them.
    let mut blocks: Vec<(String, Vec<String>)> = Vec::new();

    // The survivors are the output. Everything else is bookkeeping about how they were found, so
    // they get listed in full, each one a file and line the reader can go straight to.
    if !survivors.is_empty() && !listings.announced {
        blocks.push((
            styler.outcome(Outcome::Survived),
            survivors.iter().map(|mutant| mutant.describe()).collect(),
        ));
    }

    // A timeout counts as detected, so without this the only sign of a hang is a run that took
    // longer than it should have. They are also the mutants most worth acting on, because each one
    // is a repeated cost until it is suppressed or fixed.
    let hung: Vec<&Mutant> = plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Timeout).collect();

    if !hung.is_empty() && !listings.announced {
        blocks.push((
            styler.outcome(Outcome::Timeout),
            hung.iter()
                .map(|mutant| {
                    // The heading already says these timed out. Only a note that adds something —
                    // which test it stalled in — earns its place on the line.
                    mutant
                        .note
                        .as_deref()
                        .map_or_else(|| mutant.describe(), |note| format!("{}: {note}", mutant.describe()))
                })
                .collect(),
        ));
    }

    // A memory kill counts as detected, so like a timeout it would otherwise leave no trace beyond
    // a number. It is worth listing for a different reason: it is the outcome most likely to be
    // wrong. A ceiling that is too tight convicts a healthy mutant, and the note carries the peak
    // and the ceiling so the reader can judge which of the two happened.
    let starved: Vec<&Mutant> = plan
        .mutants
        .iter()
        .filter(|mutant| mutant.outcome == Outcome::OutOfMemory)
        .collect();

    if !starved.is_empty() && !listings.announced {
        blocks.push((
            styler.outcome(Outcome::OutOfMemory),
            starved
                .iter()
                .map(|mutant| {
                    mutant
                        .note
                        .as_deref()
                        .map_or_else(|| mutant.describe(), |note| format!("{}: {note}", mutant.describe()))
                })
                .collect(),
        ));
    }

    // Caught mutants are the bulk of a healthy run and say nothing a reader has to act on, so they
    // are listed only when asked for. Seeing them is how a user confirms the suite is testing what
    // they think it is, rather than passing for some unrelated reason.
    if listings.caught {
        let killed: Vec<String> = plan
            .mutants
            .iter()
            .filter(|mutant| mutant.outcome.is_detected())
            .map(Mutant::describe)
            .collect();

        if !killed.is_empty() {
            blocks.push((styler.outcome(Outcome::Killed), killed));
        }
    }

    // An unviable mutant is not a finding about the code, but it is not nothing either: it is
    // usually a place the encoding could not express, so naming it is what makes the gap fixable.
    // A large workspace produces thousands of them, though, and printing every one buries the
    // survivors that are the actual result, so the count on the summary line stands in for the
    // list unless the list was asked for.
    if listings.unviable {
        let unviable: Vec<String> = plan
            .mutants
            .iter()
            .filter(|mutant| mutant.outcome == Outcome::CompileError)
            .map(Mutant::describe)
            .collect();

        if !unviable.is_empty() {
            blocks.push((styler.outcome(Outcome::CompileError), unviable));
        }
    }

    let mut stream = host.output();

    for (label, lines) in &blocks {
        writeln!(stream)?;

        for line in lines {
            writeln!(stream, "{label} {line}")?;
        }
    }

    if !blocks.is_empty() {
        writeln!(stream)?;
    }

    // One line for the whole result. Everything a run knows about itself — what it built, what it
    // could not compile, what it was told to skip — is bookkeeping about how the number was
    // reached, and a reader who wants that has `--estimate` and `--advice`. What is left is the
    // number and the counts that change what it is a number out of.
    //
    // A missed mutant is one a test ran and did not notice, and nothing else. An uncovered mutant
    // also costs score, but no test reached it, so counting it as missed would send the reader
    // looking for an assertion that was never going to be there; it is named on its own instead.
    //
    // The counts are always printed, zero or not. A line whose shape depends on its contents has
    // to be read before it can be scanned, and these are the numbers a reader is looking for. They
    // sum to the population in front of them, so the line can be checked at a glance.
    //
    // Out-of-memory is named separately from caught for the same reason uncovered is named
    // separately from missed. All three of caught, timed out and out of memory count toward the
    // score, but they ask the reader to do different things: a caught mutant needs nothing, a
    // timeout is worth confirming is a real hang, and a run with out-of-memory kills in it is
    // usually telling you the ceiling is too tight rather than that the tests are good.
    if summary.valid() > 0 {
        writeln!(
            stream,
            "{heading} {} ({} caught, {} missed, {} timed out, {} out of memory, {} uncovered => {:.1}%){}",
            quantity(summary.valid() as usize, "mutant"),
            summary.killed,
            summary.survived,
            summary.timeout,
            summary.out_of_memory,
            summary.uncovered,
            summary.score(),
            excluded(plan)
        )?;
    } else {
        // Nothing was tested — a dry run, or a run every mutant was skipped out of. The population
        // is all there is to report, and reporting nothing at all would read as a failure.
        writeln!(
            stream,
            "{heading} {} in {}, none tested{}",
            quantity(plan.mutants.len(), "mutant"),
            quantity(plan.files.len(), "file"),
            excluded(plan)
        )?;
    }

    Ok(())
}

/// Reports anything about the mechanics of the run that the user has to know about.
///
/// Only the exceptional is reported. What a build cost and what budget a mutant was given are
/// answers to questions nobody asked, and `--estimate` and `--advice` exist for the runs where
/// somebody did; what is left here is the handful of things a run had to do differently from what
/// was asked of it.
///
/// This goes to the diagnostic stream, not to the results stream, because it is information about
/// the run rather than a finding about the code, and a script parsing results should not have to
/// step over it.
///
/// # Errors
///
/// Returns an error if the stream cannot be written.
pub fn session_notes<H: Host>(host: &mut H, session: &Session, styler: Styler) -> Result<()> {
    let mut stream = host.error();

    if session.widened {
        writeln!(
            stream,
            "{} the narrowed build did not compile, so the whole workspace was built; \
             a test target needing a feature another package enables cannot be built alone",
            styler.note("Scope")
        )?;
    }

    // Memory control is on by default, so the run that could not have it is the one that has to
    // say so. Saying it here rather than as progress output is deliberate: progress is suppressed
    // when nothing is watching, and an unattended CI runner without cgroup delegation is precisely
    // the case where the protection is absent and the transient line would never have been seen.
    if let Some(reason) = session.unbounded.as_ref() {
        writeln!(
            stream,
            "{} what a mutant allocates is not bounded on this host: {reason}",
            styler.note("Memory")
        )?;
    }

    if session.filtered > 0 {
        writeln!(
            stream,
            "{} {} not consulted, so a survivor here may be one they would have caught",
            styler.note("Oracle"),
            quantity(session.filtered, "test target")
        )?;
    }

    // A count on the summary line says how many, but not what to do about it, and the answer is
    // almost always a feature flag. Without this the reader sees a population smaller than the one
    // `gamma list` reported and has no way to find out why.
    if session.not_built > 0 {
        writeln!(
            stream,
            "{} {} in source the build never compiled, so no test could reach them; \
             conditional compilation is the usual reason, and `--all-features` or `--features` \
             brings that code into the run",
            styler.note("Features"),
            quantity(session.not_built, "mutant")
        )?;
    }

    // Only when it is large enough to be somebody's problem. Every run leaves something here, and
    // saying so every time would be noise; a figure that rivals the free space on a CI runner is
    // not noise, and the run that produced it is the only thing in a position to mention it.
    if session.footprint >= FOOTPRINT_NOTE {
        writeln!(
            stream,
            "{} the scratch directory holds {}; `cargo clean` reclaims it",
            styler.note("Disk"),
            bytes(session.footprint)
        )?;
    }

    Ok(())
}

/// How much scratch disk is worth mentioning.
///
/// Ten gigabytes: below that it is a normal cost of building a workspace twice, and above it the
/// figure starts to compete with the free space on a hosted CI runner.
const FOOTPRINT_NOTE: u64 = 10 * 1024 * 1024 * 1024;

/// Renders a byte count the way a person would say it.
pub(crate) fn bytes(count: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "three significant digits are printed")]
    let mut size = count as f64;

    for unit in ["bytes", "KB", "MB", "GB"] {
        if size < 1024.0 {
            return if unit == "bytes" {
                format!("{count} {unit}")
            } else {
                format!("{size:.1} {unit}")
            };
        }

        size /= 1024.0;
    }

    format!("{size:.1} TB")
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::testing::{Sink, fails_at_every_line};
    use crate::discover::TargetFile;
    use crate::model::{Mutant, Outcome};
    use crate::ops::collect::Shape;

    fn mutant(line: usize, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("m{line}"),
            ordinal: u32::try_from(line).unwrap_or(0),
            file: Utf8PathBuf::from("src/a.rs"),
            package: "subject".to_owned(),
            span: 0..1,
            line,
            column: 5,
            mutator: "relational.gt_to_ge".to_owned(),
            item_path: "subject::f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a > b".to_owned(),
            replacement: "a >= b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    fn plan() -> Plan {
        Plan {
            root: Utf8PathBuf::from("/w"),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/a.rs"),
                absolute: Utf8PathBuf::from("/w/src/a.rs"),
                package: "subject".to_owned(),
            }],
            mutants: vec![
                mutant(1, Outcome::Killed),
                mutant(2, Outcome::Survived),
                mutant(3, Outcome::Timeout),
            ],
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    fn summary(announced: bool) -> String {
        rendered(&plan(), announced)
    }

    /// Summarizes a given plan, so a test can supply its own population.
    fn rendered(plan: &Plan, announced: bool) -> String {
        let mut host = Sink::default();
        let listings = Listings {
            caught: false,
            unviable: false,
            announced,
        };

        summarize(&mut host, plan, Styler::new(false), listings).expect("summarize");

        String::from_utf8(host.out).expect("utf-8")
    }

    fn rendered_with(plan: &Plan, listings: Listings) -> String {
        let mut host = Sink::default();

        summarize(&mut host, plan, Styler::new(false), listings).expect("summarize");

        String::from_utf8(host.out).expect("utf-8")
    }

    fn session(footprint: u64, widened: bool) -> Session {
        Session {
            baseline: core::time::Duration::from_secs(1),
            quiet: core::time::Duration::ZERO,
            stall: None,
            timeout: core::time::Duration::from_secs(2),
            build: core::time::Duration::from_secs(3),
            metered: false,
            unbounded: None,
            withdrawn: 0,
            rounds: 1,
            binaries: Vec::new(),
            peak: None,
            footprint,
            filtered: 0,
            not_built: 0,
            widened,
        }
    }

    /// A narrowed oracle changes what a survivor means, so it cannot be left to the reader to guess.
    #[test]
    fn an_oracle_that_lost_test_targets_says_how_many() {
        let mut host = Sink::default();
        let narrowed = Session { filtered: 3, ..session(0, false) };

        session_notes(&mut host, &narrowed, Styler::new(false)).expect("notes");

        let printed = String::from_utf8(host.err).expect("utf-8");

        assert!(printed.contains("3 test targets not consulted"), "{printed}");
    }

    #[test]
    fn an_oracle_that_kept_every_target_says_nothing_about_it() {
        assert!(!notes(0, false).contains("Oracle"), "{}", notes(0, false));
    }

    fn notes(footprint: u64, widened: bool) -> String {
        let mut host = Sink::default();

        session_notes(&mut host, &session(footprint, widened), Styler::new(false)).expect("notes");

        String::from_utf8(host.err).expect("utf-8")
    }

    #[test]
    fn a_build_that_had_to_widen_says_so() {
        assert!(notes(0, true).contains("the whole workspace was built"), "{}", notes(0, true));
    }

    #[test]
    fn a_build_that_kept_its_scope_says_nothing_about_it() {
        assert!(!notes(0, false).contains("whole workspace"), "{}", notes(0, false));
    }

    #[test]
    fn a_scratch_directory_big_enough_to_break_a_ci_runner_is_reported() {
        // Every run leaves something behind, so only a figure that competes with a runner's free
        // space is worth a line.
        assert!(notes(FOOTPRINT_NOTE, false).contains("10.0 GB"), "{}", notes(FOOTPRINT_NOTE, false));
        assert!(!notes(FOOTPRINT_NOTE - 1, false).contains("scratch"));
    }

    #[test]
    fn byte_counts_read_the_way_a_person_would_say_them() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        assert_eq!(bytes(7 * 1024 * 1024 * 1024 * 1024), "7.0 TB");
    }

    #[test]
    fn results_are_listed_when_nothing_announced_them() {
        let text = summary(false);

        assert!(text.contains("MISSED src/a.rs:2"), "{text}");
        assert!(text.contains("TIMEOUT src/a.rs:3"), "{text}");
    }

    #[test]
    fn results_the_live_display_already_named_are_not_repeated() {
        let text = summary(true);

        assert!(!text.contains("MISSED"), "{text}");
        assert!(!text.contains("TIMEOUT"), "{text}");

        // The counts still have to be there; only the per-mutant lines are dropped.
        assert!(text.contains("3 mutants (1 caught, 1 missed, 1 timed out, 0 out of memory, 0 uncovered => 66.7%)"), "{text}");
    }

    #[test]
    fn a_memory_kill_is_listed_and_counted_separately_from_a_caught_mutant() {
        // The distinction is the point: both count toward the score, but a caught mutant needs
        // nothing from the reader and a memory kill usually means the ceiling was wrong.
        let mut plan = plan();

        plan.mutants = vec![mutant(1, Outcome::Killed), mutant(2, Outcome::OutOfMemory)];
        plan.mutants[1].note = Some("`suite` reached 200 MB, past the 150 MB this run allowed it".to_owned());

        let text = rendered(&plan, false);

        assert!(text.contains("OUTOFMEM"), "{text}");
        assert!(text.contains("past the 150 MB"), "{text}");
        assert!(text.contains("1 caught, 0 missed, 0 timed out, 1 out of memory"), "{text}");
    }

    #[test]
    fn a_run_that_could_not_bound_memory_says_so_on_the_diagnostic_stream() {
        // This note has to survive a non-terminal run, because a CI runner without cgroup
        // delegation is exactly the case where the protection is missing and nobody is watching.
        let mut host = Sink::default();
        let mut settled = session(0, false);

        settled.unbounded = Some("no cgroup delegation".to_owned());

        session_notes(&mut host, &settled, Styler::new(false)).expect("notes");

        let text = String::from_utf8(host.err).expect("utf-8");

        assert!(text.contains("not bounded on this host: no cgroup delegation"), "{text}");
    }

    #[test]
    fn one_of_something_is_singular() {
        assert_eq!(quantity(1, "file"), "1 file");
        assert_eq!(quantity(1, "build round"), "1 build round");
    }

    #[test]
    fn any_other_count_is_plural() {
        assert_eq!(quantity(0, "file"), "0 files");
        assert_eq!(quantity(2, "mutant"), "2 mutants");
    }

    #[test]
    fn a_continuation_is_exactly_the_status_column() {
        assert_eq!(continuation().len(), VERB_WIDTH);
        assert!(continuation().chars().all(char::is_whitespace));
    }

    #[test]
    fn the_test_sink_exposes_both_streams_without_claiming_a_terminal() {
        let mut host = Sink::default();

        // Report helpers use the same host trait as the CLI, so the local double should exercise
        // both streams and the non-terminal answers instead of relying on dead methods.
        let _ = host.output().write_all(b"out");
        let _ = host.error().write_all(b"err");

        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
        assert_eq!(String::from_utf8(host.out).expect("utf-8"), "out");
        assert_eq!(String::from_utf8(host.err).expect("utf-8"), "err");
    }

    #[test]
    fn the_summary_still_names_mutants_already_settled_out_of_the_run() {
        let mut plan = plan();

        plan.settled_out = 5;

        let text = rendered(&plan, false);

        // Settled mutants were deliberately excluded from this run, and that changes the
        // denominator a reader would otherwise infer from the workspace.
        assert!(text.contains("5 already settled"), "{text}");
    }

    #[test]
    fn caught_mutants_are_listed_only_when_requested() {
        let listings = Listings {
            caught: true,
            unviable: false,
            announced: true,
        };

        let text = rendered_with(&plan(), listings);

        // Caught mutants are usually noise, but verbose listings use them to prove the suite ran
        // the mutation the reader expected.
        assert!(text.contains("caught src/a.rs:1"), "{text}");
        assert!(!text.contains("MISSED src/a.rs:2"), "{text}");
    }

    #[test]
    fn an_empty_population_reports_files_instead_of_a_score() {
        let mut plan = plan();

        plan.mutants.clear();

        let text = rendered(&plan, false);

        // With no scored mutants, a percentage would be a fiction; the useful fact is the selected
        // population size.
        assert!(text.contains("0 mutants in 1 file, none tested"), "{text}");
    }

    #[test]
    fn a_timeout_is_not_told_it_timed_out_twice() {
        // The heading already says TIMEOUT, so restating it on every line is noise on the listing
        // most likely to be long.
        let mut plan = plan();

        plan.mutants = vec![mutant(7, Outcome::Timeout)];

        let text = rendered(&plan, false);

        assert!(text.contains("src/a.rs:7"), "{text}");
        assert!(!text.contains("ran out its budget"), "{text}");

        // Nothing follows the mutant on the line, so it must not end on a dangling colon.
        assert!(!text.contains("[relational.gt_to_ge]:"), "{text}");
    }

    #[test]
    fn a_stalled_mutant_still_names_the_test_it_hung_in() {
        // This note is the whole reason the field exists: which test stopped making progress is
        // the one thing a reader cannot work out from the mutant itself.
        let mut hung = mutant(7, Outcome::Timeout);

        hung.note = Some("stalled, last test named was `t_slow`".to_owned());

        let mut plan = plan();

        plan.mutants = vec![hung];

        let text = rendered(&plan, false);

        assert!(text.contains("stalled, last test named was `t_slow`"), "{text}");
    }

    #[test]
    fn the_summary_does_not_count_mutants_the_compiler_rejected() {
        // A large workspace produces thousands, they are withdrawn automatically, and no reader
        // acts on the number. It has no place on the one line everybody reads.
        let mut plan = plan();

        plan.mutants.push(mutant(4, Outcome::CompileError));

        let text = rendered(&plan, false);

        assert!(!text.contains("unviable"), "{text}");
    }

    #[test]
    fn the_summary_still_names_mutants_that_were_deliberately_held_back() {
        let mut plan = plan();

        plan.suppressed = 3;
        plan.sharded_out = 9;

        let text = rendered(&plan, false);

        assert!(text.contains("3 suppressed"), "{text}");
        assert!(text.contains("9 outside this shard"), "{text}");
    }

    /// A closed pipe on the results stream has to surface, not be swallowed part-way through.
    #[test]
    fn a_closed_results_stream_fails_the_summary() {
        let listings = Listings {
            caught: true,
            unviable: true,
            announced: true,
        };

        fails_at_every_line(5, |host| summarize(host, &plan(), Styler::new(false), listings));
    }

    /// The same on the population line taken when nothing was actually tested.
    #[test]
    fn a_closed_results_stream_fails_the_untested_summary() {
        let mut plan = plan();

        plan.mutants.clear();

        let listings = Listings {
            caught: false,
            unviable: false,
            announced: false,
        };

        fails_at_every_line(1, |host| summarize(host, &plan, Styler::new(false), listings));
    }

    /// And on the diagnostic stream carrying the session notes.
    #[test]
    fn a_closed_diagnostic_stream_fails_the_session_notes() {
        fails_at_every_line(1, |host| session_notes(host, &session(0, true), Styler::new(false)));
    }

}
