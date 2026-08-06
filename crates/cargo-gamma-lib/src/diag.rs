//! A dump of everything a run measured about itself, for people working on the tool.
//!
//! This is not [`crate::advise`]. Advice is written for someone whose run was slow and who wants
//! to know what to do about it, so it withholds anything they cannot act on. This withholds
//! nothing, is not stable, and is not documented in the README: it exists so that a change to the
//! scheduler, the build sequencing or the mutator catalog can be judged against numbers rather
//! than against how the run felt.
//!
//! It goes to the diagnostic stream, so `--diag` composes with piping the results somewhere.

use core::cmp::Reverse;
use core::fmt::Write as _;
use core::time::Duration;

use crate::HashMap;
use crate::advise::human;
use crate::discover::Plan;
use crate::exec::Session;
use crate::exec::TestBinary;
use crate::model::{Mutant, Outcome, Summary};
use crate::report::quantity;

/// How many rows the "worst offender" tables keep.
///
/// Long enough to show a pattern rather than a single outlier, short enough that the whole dump
/// still fits on a screen next to the run that produced it.
const TOP: usize = 10;

/// One row of a per-something breakdown.
#[derive(Debug, Default)]
struct Bucket {
    mutants: usize,
    cpu: Duration,
    survivors: usize,
    unviable: usize,
}

impl Bucket {
    /// Folds one mutant into the tally.
    fn absorb(&mut self, mutant: &Mutant) {
        self.mutants += 1;
        self.cpu += Duration::from_millis(mutant.elapsed_ms);

        match mutant.outcome {
            Outcome::Survived => self.survivors += 1,
            Outcome::CompileError => self.unviable += 1,
            _other => {}
        }
    }
}

/// Renders the whole dump.
///
/// `session` is absent when nothing was live, so nothing was built or measured; the population is
/// still worth reporting, because a run that found no work is exactly the kind that wants
/// explaining.
#[must_use]
pub fn render(plan: &Plan, session: Option<&Session>, jobs: usize, wall: Duration) -> String {
    let summary = Summary::of(&plan.mutants);
    let mut text = String::new();

    let _ = writeln!(text, "── diag ──────────────────────────────────────────────");

    // The build and the baseline are the run's fixed cost, so what is left is the only part that
    // scales with the population and the only part worth judging the scheduler on.
    let fixed = session.map_or(Duration::ZERO, |session| session.build + session.baseline);
    let testing = wall.saturating_sub(fixed);

    let _ = writeln!(
        text,
        "run       wall {}, of which {} testing, {} jobs",
        human(wall),
        human(testing),
        jobs
    );

    let _ = writeln!(text, "          root {}", plan.root);

    let _ = writeln!(
        text,
        "discover  {}, {}, {}, {}",
        quantity(plan.files.len(), "file"),
        quantity(plan.mutants.len(), "mutant"),
        quantity(plan.reach.len(), "package"),
        quantity(plan.reach.values().map(crate::HashSet::len).sum::<usize>(), "reach edge")
    );

    let _ = writeln!(
        text,
        "          withheld: {} suppressed, {} out of shard, {} already settled",
        plan.suppressed, plan.sharded_out, plan.settled_out
    );

    let _ = writeln!(
        text,
        "outcomes  {} killed, {} survived, {} timeout, {} unviable, {} uncovered, {} ignored, {} pending",
        summary.killed, summary.survived, summary.timeout, summary.unviable, summary.uncovered, summary.ignored,
        summary.pending
    );

    if let Some(session) = session {
        write_session(&mut text, session);
    }

    write_throughput(&mut text, &plan.mutants, jobs, testing);
    write_slowest(&mut text, &plan.mutants);
    write_breakdown(&mut text, "mutator", &group(&plan.mutants, |mutant| mutant.mutator.clone()));
    write_breakdown(&mut text, "package", &group(&plan.mutants, |mutant| mutant.package.clone()));
    write_breakdown(&mut text, "file", &group(&plan.mutants, |mutant| mutant.file.to_string()));

    if let Some(session) = session {
        write_binaries(&mut text, session);
    }

    text
}

/// Reports what the fixed cost of the run was, and what it bought.
fn write_session(text: &mut String, session: &Session) {
    let _ = writeln!(
        text,
        "build     {} over {} rounds, {} withdrawn, selection {}",
        human(session.build),
        session.rounds,
        session.withdrawn,
        if session.widened { "widened to the workspace" } else { "kept" }
    );

    let _ = writeln!(
        text,
        "baseline  {}, longest silence {}, mutant timeout {}, stall budget {}",
        human(session.baseline),
        human(session.quiet),
        human(session.timeout),
        session.stall.map_or_else(|| "off".to_owned(), human)
    );

    let _ = writeln!(
        text,
        "memory    baseline peak {}",
        session.peak.map_or_else(|| "not measured".to_owned(), crate::report::bytes)
    );
}

/// Reports how well the run kept its workers busy.
///
/// The number that matters is the effective job count: CPU over wall. A scheduler that is working
/// lands within a fraction of `--jobs`, and everything short of that is time spent waiting for the
/// slowest binary of a batch rather than testing anything.
fn write_throughput(text: &mut String, mutants: &[Mutant], jobs: usize, testing: Duration) {
    let mut spent: Vec<Duration> = mutants
        .iter()
        .filter(|mutant| mutant.elapsed_ms > 0)
        .map(|mutant| Duration::from_millis(mutant.elapsed_ms))
        .collect();

    if spent.is_empty() {
        let _ = writeln!(text, "mutants   nothing was run");
        return;
    }

    spent.sort_unstable();

    let cpu: Duration = spent.iter().sum();

    let _ = writeln!(
        text,
        "mutants   {} evaluated, {} cpu, {} effective jobs of {jobs}",
        spent.len(),
        human(cpu),
        ratio(cpu, testing)
    );

    let _ = writeln!(
        text,
        "          min {}, p50 {}, p90 {}, p99 {}, max {}",
        human(spent[0]),
        human(percentile(&spent, 0.50)),
        human(percentile(&spent, 0.90)),
        human(percentile(&spent, 0.99)),
        human(spent[spent.len() - 1])
    );
}

/// Names the mutants that cost the most, which is where a scheduling change shows up first.
fn write_slowest(text: &mut String, mutants: &[Mutant]) {
    let mut ranked: Vec<&Mutant> = mutants.iter().filter(|mutant| mutant.elapsed_ms > 0).collect();

    if ranked.is_empty() {
        return;
    }

    ranked.sort_unstable_by_key(|mutant| Reverse(mutant.elapsed_ms));
    ranked.truncate(TOP);

    let _ = writeln!(text, "\nslowest mutants");

    for mutant in ranked {
        let _ = writeln!(
            text,
            "  {:>8}  {:<9} {}",
            human(Duration::from_millis(mutant.elapsed_ms)),
            label(mutant.outcome),
            mutant.describe()
        );
    }
}

/// Reports one grouping, ranked by the CPU it consumed.
///
/// Ranked by cost rather than by name because the question this table answers is always "what
/// should be looked at first", and the answer is whatever is at the top.
fn write_breakdown(text: &mut String, noun: &str, buckets: &HashMap<String, Bucket>) {
    if buckets.is_empty() {
        return;
    }

    let mut rows: Vec<(&String, &Bucket)> = buckets.iter().collect();

    rows.sort_by(|(left_name, left), (right_name, right)| {
        right.cpu.cmp(&left.cpu).then_with(|| left_name.cmp(right_name))
    });

    let shown = rows.len().min(TOP);

    let _ = writeln!(text, "\nby {noun} ({shown} of {})", rows.len());
    let _ = writeln!(text, "  {:>8}  {:>7}  {:>9}  {:>8}  {noun}", "cpu", "mutants", "survivors", "unviable");

    for (name, bucket) in rows.into_iter().take(TOP) {
        let _ = writeln!(
            text,
            "  {:>8}  {:>7}  {:>9}  {:>8}  {name}",
            human(bucket.cpu),
            bucket.mutants,
            bucket.survivors,
            bucket.unviable
        );
    }
}

/// Reports what each test binary cost the run.
///
/// A binary's baseline is charged to every mutant that can reach it, so a single slow one is
/// multiplied by the population and is the most leveraged thing in a run.
fn write_binaries(text: &mut String, session: &Session) {
    if session.binaries.is_empty() {
        return;
    }

    let mut binaries: Vec<&TestBinary> = session.binaries.iter().collect();

    binaries.sort_by_key(|binary| Reverse(binary.baseline));

    let total: Duration = binaries.iter().map(|binary| binary.baseline).sum();

    let _ = writeln!(text, "\ntest binaries ({}, {} baseline)", binaries.len(), human(total));
    let _ = writeln!(
        text,
        "  {:>8}  {:>8}  {:>10}  {:>10}  {:<20} binary",
        "baseline", "budget", "peak", "ceiling", "package"
    );

    for binary in binaries.into_iter().take(TOP) {
        let _ = writeln!(
            text,
            "  {:>8}  {:>8}  {:>10}  {:>10}  {:<20} {}",
            human(binary.baseline),
            human(binary.budget),
            binary.peak.map_or_else(|| "-".to_owned(), crate::report::bytes),
            binary.memory.map_or_else(|| "-".to_owned(), crate::report::bytes),
            binary.package,
            binary.path.file_name().unwrap_or(binary.path.as_str())
        );
    }
}

/// Buckets the population by whatever `key` names.
fn group(mutants: &[Mutant], key: impl Fn(&Mutant) -> String) -> HashMap<String, Bucket> {
    let mut buckets: HashMap<String, Bucket> = HashMap::default();

    for mutant in mutants {
        buckets.entry(key(mutant)).or_default().absorb(mutant);
    }

    buckets
}

/// The value at `fraction` through an already-sorted list.
fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }

    #[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
    let position = fraction * (sorted.len() - 1) as f64;

    #[expect(clippy::cast_possible_truncation, reason = "the operand is an index into the list above")]
    #[expect(clippy::cast_sign_loss, reason = "the fraction and the length are both non-negative")]
    let index = position.round() as usize;

    sorted[index.min(sorted.len() - 1)]
}

/// How many workers the run actually kept busy, as a printable ratio.
fn ratio(cpu: Duration, wall: Duration) -> String {
    if wall.is_zero() {
        return "?".to_owned();
    }

    format!("{:.1}", cpu.as_secs_f64() / wall.as_secs_f64())
}

/// The unstyled name of an outcome, so the columns line up whatever the terminal is.
const fn label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Killed => "killed",
        Outcome::Survived => "missed",
        Outcome::Timeout => "timeout",
        Outcome::OutOfMemory => "outofmem",
        Outcome::CompileError => "unviable",
        Outcome::Ignored => "ignored",
        Outcome::NoCoverage => "uncovered",
        Outcome::NotBuilt => "notbuilt",
        Outcome::Pending => "pending",
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::ops::collect::Shape;

    fn mutant(file: &str, mutator: &str, outcome: Outcome, ms: u64) -> Mutant {
        Mutant {
            id: format!("{file}:{mutator}:{ms}"),
            ordinal: 1,
            file: Utf8PathBuf::from(file),
            package: "subject".to_owned(),
            span: 0..1,
            line: 1,
            column: 1,
            mutator: mutator.to_owned(),
            item_path: "f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a + b".to_owned(),
            replacement: "a - b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: ms,
            killed_by: None,
            note: None,
        }
    }

    fn plan(mutants: Vec<Mutant>) -> Plan {
        Plan {
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants,
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    fn session(binaries: Vec<TestBinary>) -> Session {
        Session {
            baseline: Duration::from_secs(5),
            quiet: Duration::from_secs(2),
            stall: Some(Duration::from_secs(3)),
            timeout: Duration::from_secs(30),
            build: Duration::from_secs(7),
            metered: false,
            unbounded: None,
            withdrawn: 4,
            rounds: 2,
            binaries,
            peak: None,
            footprint: 0,
            filtered: 0,
            not_built: 0,
            widened: true,
        }
    }

    #[test]
    fn a_percentile_never_indexes_past_the_end() {
        let sorted = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(3)];

        assert_eq!(percentile(&sorted, 0.0), Duration::from_secs(1));
        assert_eq!(percentile(&sorted, 1.0), Duration::from_secs(3));
        assert_eq!(percentile(&sorted, 0.5), Duration::from_secs(2));
    }

    #[test]
    fn a_percentile_of_nothing_is_zero_rather_than_a_panic() {
        assert_eq!(percentile(&[], 0.9), Duration::ZERO);
    }

    #[test]
    fn effective_jobs_is_cpu_over_wall() {
        assert_eq!(ratio(Duration::from_secs(80), Duration::from_secs(10)), "8.0");
    }

    #[test]
    fn a_run_with_no_wall_time_reports_no_ratio_rather_than_an_infinite_one() {
        assert_eq!(ratio(Duration::from_secs(80), Duration::ZERO), "?");
    }

    #[test]
    fn a_breakdown_is_ranked_by_cost() {
        let mutants = vec![
            mutant("a.rs", "arith.add_to_sub", Outcome::Killed, 100),
            mutant("b.rs", "literal.int_to_zero", Outcome::Survived, 900),
        ];

        let text = render(&plan(mutants), None, 4, Duration::from_secs(1));
        let cheap = text.find("arith.add_to_sub").expect("the cheap family is listed");
        let dear = text.find("literal.int_to_zero").expect("the expensive family is listed");

        assert!(dear < cheap, "the expensive family must come first:\n{text}");
    }

    #[test]
    fn a_run_that_tested_nothing_still_reports_its_population() {
        let text = render(&plan(vec![mutant("a.rs", "arith.add_to_sub", Outcome::Pending, 0)]), None, 4, Duration::ZERO);

        assert!(text.contains("1 mutant,"), "{text}");
        assert!(text.contains("nothing was run"), "{text}");

        // With no session there is nothing to say about a build that never happened, and inventing
        // zeroes for it would read as a build that took no time.
        assert!(!text.contains("baseline"), "{text}");
    }

    #[test]
    fn a_live_run_reports_the_session_costs_and_scope() {
        let text = render(&plan(Vec::new()), Some(&session(Vec::new())), 4, Duration::from_secs(20));

        // These fixed costs explain how much of the wall clock was not mutant execution.
        assert!(
            text.contains("build     7.0s over 2 rounds, 4 withdrawn, selection widened to the workspace"),
            "{text}"
        );
        assert!(
            text.contains("baseline  5.0s, longest silence 2.0s, mutant timeout 30.0s, stall budget 3.0s"),
            "{text}"
        );
    }

    #[test]
    fn a_session_without_a_stall_budget_says_it_is_off() {
        let mut session = session(Vec::new());

        session.stall = None;
        session.widened = false;

        let text = render(&plan(Vec::new()), Some(&session), 4, Duration::from_secs(20));

        // The absence of a stall budget is a real operating mode, not a zero-second timeout.
        assert!(text.contains("selection kept"), "{text}");
        assert!(text.contains("stall budget off"), "{text}");
    }

    #[test]
    fn a_breakdown_counts_unviable_mutants_separately_from_survivors() {
        let text = render(
            &plan(vec![
                mutant("a.rs", "arith.add_to_sub", Outcome::CompileError, 10),
                mutant("a.rs", "arith.add_to_sub", Outcome::Survived, 20),
            ]),
            None,
            4,
            Duration::from_secs(1),
        );

        // Unviable mutants are withdrawn from the score, but the diagnostic table keeps their cost
        // visible to someone improving the mutator.
        assert!(text.contains("outcomes  0 killed, 1 survived, 0 timeout, 1 unviable"), "{text}");
        assert!(text.contains("      30ms        2          1         1  arith.add_to_sub"), "{text}");
    }

    #[test]
    fn the_slowest_table_uses_plain_outcome_labels_for_every_non_survivor_result() {
        let text = render(
            &plan(vec![
                mutant("timeout.rs", "m", Outcome::Timeout, 70),
                mutant("unviable.rs", "m", Outcome::CompileError, 60),
                mutant("ignored.rs", "m", Outcome::Ignored, 50),
                mutant("uncovered.rs", "m", Outcome::NoCoverage, 40),
                mutant("pending.rs", "m", Outcome::Pending, 30),
            ]),
            None,
            4,
            Duration::from_secs(1),
        );

        // The table is intentionally unstyled, so every verdict has to be rendered as text that
        // still lines up in a plain diagnostic dump.
        for label in ["timeout", "unviable", "ignored", "uncovered", "pending"] {
            assert!(text.contains(label), "{text}");
        }
    }

    #[test]
    fn test_binaries_are_ranked_by_baseline_cost() {
        let text = render(
            &plan(Vec::new()),
            Some(&session(vec![
                TestBinary {
                    package: "fast".to_owned(),
                    baseline: Duration::from_secs(1),
                    budget: Duration::from_secs(10),
                    ..crate::testing::test_binary("/w/target/debug/deps/fast-abc")
                },
                TestBinary {
                    package: "slow".to_owned(),
                    baseline: Duration::from_secs(3),
                    budget: Duration::from_secs(30),
                    ..crate::testing::test_binary("/w/target/debug/deps/slow-def")
                },
            ])),
            4,
            Duration::from_secs(20),
        );

        let slow = text.find("slow-def").expect("slow binary is listed");
        let fast = text.find("fast-abc").expect("fast binary is listed");

        // A slow binary is multiplied by every mutant that reaches it, so the most expensive one
        // must be shown first.
        assert!(text.contains("test binaries (2, 4.0s baseline)"), "{text}");
        assert!(slow < fast, "{text}");
    }
}
