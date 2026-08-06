//! Diagnosis of a mutation run: where the time went, and what can be done about it.
//!
//! Mutation testing is the kind of tool that gets adopted enthusiastically, runs for four hours,
//! and is then quietly deleted from the CI configuration. The run itself does not explain why it
//! was slow, so the only remedies available to a frustrated user are the blunt ones — fewer
//! operators, fewer files, or nothing at all — chosen without knowing what they cost in signal.
//!
//! This module turns a completed run into a list of findings. Each is a measured symptom, a named
//! cause, a remedy, and the signal cost of taking that remedy. The last part is the one that
//! matters: every mitigation here trades information for time, and a recommendation that hides the
//! trade is worse than no recommendation, because it will be taken.

mod finding;
mod timing;
mod yield_;

use core::cmp::Ordering;
use core::fmt::Write as _;
use core::time::Duration;

use crate::HashMap;
use crate::model::{Mutant, Outcome, Summary};

pub use finding::Finding;
pub use timing::Timing;
pub use yield_::Yield;

/// The heading of the section reporting what the run cost and decided.
const RUN_HEADING: &str = "This run";

/// The heading of the section holding the diagnoses.
const FINDINGS_HEADING: &str = "Findings";

/// The heading of the per-family cost and value table.
const YIELD_HEADING: &str = "Yield by mutator family";

/// The heading of the definitions.
const GLOSSARY_HEADING: &str = "What the verdicts mean";

/// The fraction of the population one file must hold before it is worth naming.
const HOT_FILE_SHARE: f64 = 0.10;

/// The population below which share-based findings are arithmetic rather than evidence.
///
/// In a run of four mutants every file is a hot file and every family is a quarter of the budget.
/// Reporting that is not a smaller version of the real finding, it is a different and false one,
/// and a tool that cries wolf on a toy project is one nobody reads on a real one.
const MIN_POPULATION: usize = 50;

/// The CPU time a family must consume before its yield is worth judging at all.
///
/// A share threshold alone would flag a family that used 40% of six seconds.
const MIN_YIELD_CPU: Duration = Duration::from_secs(60);

/// The fraction of mutant execution time a family must consume before its yield is worth judging.
///
/// Below this, a family with no survivors is not a problem: disabling it would save nothing, and
/// the advice would be pure noise on a report someone has to read.
const YIELD_FLOOR_SHARE: f64 = 0.05;

/// The fraction of wall time the fixed cost must exceed before it is the thing to fix.
const FIXED_COST_SHARE: f64 = 0.30;

/// The fraction of the population that must be unviable before rollback is worth reporting.
const UNVIABLE_SHARE: f64 = 0.05;

/// The fraction of valid mutants that must be uncovered before it is the headline.
const UNCOVERED_SHARE: f64 = 0.10;

/// The baseline duration above which every mutant is paying a noticeable fixed cost.
const SLOW_BASELINE: Duration = Duration::from_secs(10);

/// The wall time above which a run will not fit in a routine CI job.
const LONG_RUN: Duration = Duration::from_secs(30 * 60);

/// The wall time a shard should aim for, used to size the suggested rotation.
const TARGET_SHARD: Duration = Duration::from_secs(15 * 60);

/// Analyzes a completed run.
///
/// Findings come back in the order they should be read, which is neither the order they are
/// computed in nor severity order: the ones whose remedy costs no signal come first, so that a
/// reader who stops early stops having seen the free wins rather than the expensive ones.
#[must_use]
pub fn analyze(mutants: &[Mutant], timing: &Timing) -> Vec<Finding> {
    let mut findings = Vec::new();
    let summary = Summary::of(mutants);
    let executed: Duration = mutants.iter().map(|mutant| Duration::from_millis(mutant.elapsed_ms)).sum();

    if let Some(finding) = fixed_cost(timing) {
        findings.push(finding);
    }

    if let Some(finding) = slow_baseline(timing, mutants) {
        findings.push(finding);
    }

    if let Some(finding) = long_run(timing) {
        findings.push(finding);
    }

    if let Some(finding) = timeouts(summary, mutants) {
        findings.push(finding);
    }

    if let Some(finding) = unviable(summary) {
        findings.push(finding);
    }

    findings.extend(hot_files(mutants));
    findings.extend(low_yield(mutants, executed));

    if let Some(finding) = uncovered(summary) {
        findings.push(finding);
    }

    findings
}

/// Reports the cost and value of each mutator family, worst ratio last.
#[must_use]
pub fn yields(mutants: &[Mutant]) -> Vec<Yield> {
    let mut buckets: HashMap<String, Yield> = HashMap::default();

    for mutant in mutants {
        let family = family_of(&mutant.mutator).to_owned();
        let entry = buckets.entry(family.clone()).or_insert_with(|| Yield {
            family,
            mutants: 0,
            cpu: Duration::ZERO,
            survivors: 0,
        });

        entry.mutants += 1;
        entry.cpu += Duration::from_millis(mutant.elapsed_ms);

        if mutant.outcome == Outcome::Survived {
            entry.survivors += 1;
        }
    }

    let mut rows: Vec<Yield> = buckets.into_values().collect();

    rows.sort_by(|left, right| {
        right
            .per_cpu_hour()
            .partial_cmp(&left.per_cpu_hour())
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.family.cmp(&right.family))
    });

    rows
}

/// Pluralizes a count's noun, because "1 mutants" reads as a bug in the tool.
fn plural(count: u32, noun: &str) -> String {
    if count == 1 { noun.to_owned() } else { format!("{noun}s") }
}

/// The family part of a mutator name: everything before the first dot.
fn family_of(mutator: &str) -> &str {
    mutator.split_once('.').map_or(mutator, |(family, _)| family)
}

/// A build that costs more than the testing it enables.
fn fixed_cost(timing: &Timing) -> Option<Finding> {
    let wall = timing.wall.as_secs_f64();
    let fixed = timing.build.as_secs_f64() + timing.baseline.as_secs_f64();

    if wall <= 0.0 || fixed / wall < FIXED_COST_SHARE {
        return None;
    }

    Some(Finding {
        code: "fixed-cost",
        headline: format!(
            "{:.0}% of the run was the build and baseline, not mutation testing",
            fixed / wall * 100.0
        ),
        detail: vec![
            format!("build {}, baseline {}", human(timing.build), human(timing.baseline)),
            format!("mutant execution {}", human(timing.wall.saturating_sub(timing.build + timing.baseline))),
        ],
        remedy: "test more mutants per build: widen `--ops`, or drop `--shard-count` so each run \
                 amortizes the build over more work. A build cache such as sccache helps the build \
                 itself."
            .to_owned(),
        cost: "none — this is the one finding whose remedy costs no signal at all".to_owned(),
    })
}

/// A suite whose fixed per-run cost is paid once per mutant.
#[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
fn slow_baseline(timing: &Timing, mutants: &[Mutant]) -> Option<Finding> {
    if timing.baseline < SLOW_BASELINE {
        return None;
    }

    let live = mutants.iter().filter(|mutant| mutant.outcome.is_valid()).count();
    let projected = timing.baseline.mul_f64(live as f64).div_f64(timing.jobs.max(1) as f64);

    Some(Finding {
        code: "slow-baseline",
        headline: format!("the suite takes {} with no mutant active", human(timing.baseline)),
        detail: vec![
            format!("every one of the {live} tested mutants pays that cost"),
            format!("floor for this run at {} jobs: {}", timing.jobs, human(projected)),
        ],
        remedy: "the run cannot be faster than this without a faster suite. Look for a fixture \
                 built once per test process, a sleep, or a network call; and check whether the \
                 slowest test binary is one a mutation run needs at all."
            .to_owned(),
        cost: "none — making the suite faster costs nothing and helps every other workflow"
            .to_owned(),
    })
}

/// A run too long to sit in a routine CI job.
fn long_run(timing: &Timing) -> Option<Finding> {
    if timing.wall < LONG_RUN {
        return None;
    }

    let shards = (timing.wall.as_secs_f64() / TARGET_SHARD.as_secs_f64()).ceil().max(2.0);

    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "bounded by the ratio of two durations")]
    let shards = shards as u32;

    Some(Finding {
        code: "long-run",
        headline: format!("the run took {}, which will not fit a per-commit job", human(timing.wall)),
        detail: vec![format!(
            "at {} per shard that is a rotation of {shards}",
            human(TARGET_SHARD)
        )],
        remedy: format!(
            "run one shard a night — `--shard-count {shards} --shard-index <n>` — and combine the \
             reports with `cargo gamma merge`. Shards are assigned by content, so coverage \
             accumulates as the code changes instead of resetting."
        ),
        cost: "none in total coverage, but a verdict is up to one rotation old rather than current"
            .to_owned(),
    })
}

/// Mutants that hung, and the budget they burned proving it.
fn timeouts(summary: Summary, mutants: &[Mutant]) -> Option<Finding> {
    if summary.timeout == 0 {
        return None;
    }

    let spent: Duration = mutants
        .iter()
        .filter(|mutant| mutant.outcome == Outcome::Timeout)
        .map(|mutant| Duration::from_millis(mutant.elapsed_ms))
        .sum();

    Some(Finding {
        code: "timeouts",
        headline: format!("{} {} ran out their whole budget", summary.timeout, plural(summary.timeout, "mutant")),
        detail: vec![format!("{} of CPU time spent waiting for them", human(spent))],
        remedy: "a mutant that hangs is a mutant the suite detected, so this is signal, not \
                 failure — it is just expensive signal. `cargo gamma suppress --eligible timeout` \
                 writes suppressions for them so the next run does not pay again."
            .to_owned(),
        cost: "a suppressed timeout leaves the score unchanged today, but stops being retested, so \
               a later edit that makes it terminate goes unnoticed"
            .to_owned(),
    })
}

/// Mutants that could not be compiled, and the rebuild rounds they forced.
fn unviable(summary: Summary) -> Option<Finding> {
    let total = summary.valid() + summary.unviable + summary.ignored;

    if usize::try_from(total).unwrap_or(usize::MAX) < MIN_POPULATION || f64::from(summary.unviable) / f64::from(total) < UNVIABLE_SHARE {
        return None;
    }

    Some(Finding {
        code: "unviable",
        headline: format!(
            "{} of {total} mutants could not compile ({:.0}%)",
            summary.unviable,
            f64::from(summary.unviable) / f64::from(total) * 100.0
        ),
        detail: vec![
            "each withdrawal round is a full rebuild of the instrumented tree".to_owned(),
            "they are excluded from the score, so the cost bought nothing".to_owned(),
        ],
        remedy: "`cargo gamma suppress --eligible unviable` records them in the source so later runs \
                 skip them without discovering their unviability again. If they cluster in one \
                 operator, narrow `--ops` instead."
            .to_owned(),
        cost: "none — an unviable mutant never contributed to the score".to_owned(),
    })
}

/// Files holding an outsized share of the population.
#[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
fn hot_files(mutants: &[Mutant]) -> Vec<Finding> {
    let total = mutants.len();

    if total < MIN_POPULATION {
        return Vec::new();
    }

    let mut counts: HashMap<&str, (u32, u32, Duration)> = HashMap::default();

    for mutant in mutants {
        let entry = counts.entry(mutant.file.as_str()).or_insert((0, 0, Duration::ZERO));

        entry.0 += 1;
        entry.2 += Duration::from_millis(mutant.elapsed_ms);

        if mutant.outcome == Outcome::Survived {
            entry.1 += 1;
        }
    }

    let mut hot: Vec<(&str, u32, u32, Duration)> = counts
        .into_iter()
        .filter(|&(_, (count, _, _))| f64::from(count) / total as f64 >= HOT_FILE_SHARE)
        .map(|(file, (count, survivors, cpu))| (file, count, survivors, cpu))
        .collect();

    hot.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));

    hot.into_iter()
        .map(|(file, count, survivors, cpu)| Finding {
            code: "hot-file",
            headline: format!(
                "{file} alone is {:.0}% of the population ({count} {})",
                f64::from(count) / total as f64 * 100.0,
                plural(count, "mutant")
            ),
            detail: vec![format!("{} of CPU time, {survivors} {} found there", human(cpu), plural(survivors, "survivor"))],
            remedy: "if it is generated, tabular or macro-expanded code, exclude it with \
                     `--exclude-file` or the `exclude-files` config key. If it is hand-written, \
                     this is not a problem — it is where the logic is."
                .to_owned(),
            cost: format!(
                "exactly {count} {} stop being tested, {survivors} of which are currently finding \
                 gaps in the suite",
                plural(count, "mutant")
            ),
        })
        .collect()
}

/// Families spending real time and finding nothing.
fn low_yield(mutants: &[Mutant], executed: Duration) -> Vec<Finding> {
    if executed.is_zero() {
        return Vec::new();
    }

    yields(mutants)
        .into_iter()
        .filter(|row| row.survivors == 0)
        .filter(|row| row.cpu >= MIN_YIELD_CPU)
        .filter(|row| row.cpu.as_secs_f64() / executed.as_secs_f64() >= YIELD_FLOOR_SHARE)
        .map(|row| Finding {
            code: "low-yield",
            headline: format!(
                "the `{}` family spent {} and found no survivors",
                row.family,
                human(row.cpu)
            ),
            detail: vec![format!(
                "{} {}, {:.0}% of mutant execution time",
                row.mutants,
                plural(row.mutants, "mutant"),
                row.cpu.as_secs_f64() / executed.as_secs_f64() * 100.0
            )],
            remedy: format!("`--ops 'all,!{}'` drops it", row.family),
            cost: "real, and easy to underrate. A family that finds nothing today is a regression \
                   detector for tomorrow: this says the suite currently covers it, not that it \
                   always will"
                .to_owned(),
        })
        .collect()
}

/// Code no test reaches at all.
fn uncovered(summary: Summary) -> Option<Finding> {
    let valid = summary.valid();

    if usize::try_from(valid).unwrap_or(usize::MAX) < MIN_POPULATION || f64::from(summary.uncovered) / f64::from(valid) < UNCOVERED_SHARE {
        return None;
    }

    Some(Finding {
        code: "uncovered",
        headline: format!(
            "{} of {valid} mutants ({:.0}%) sit in code no test reaches",
            summary.uncovered,
            f64::from(summary.uncovered) / f64::from(valid) * 100.0
        ),
        detail: vec!["they count against the score, because untested code is the finding".to_owned()],
        remedy: "this is not a performance problem and there is nothing to tune. Write tests, or \
                 delete the code."
            .to_owned(),
        cost: "—".to_owned(),
    })
}

/// Renders findings as plain text, in the order [`analyze`] produced them.
#[must_use]
pub fn render(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "nothing to advise: no measurement in this run crossed a threshold worth acting on\n"
            .to_owned();
    }

    let mut out = String::new();

    for finding in findings {
        let _ = writeln!(out, "{}: {}", finding.code, finding.headline);

        for line in &finding.detail {
            let _ = writeln!(out, "    {line}");
        }

        let _ = writeln!(out, "    remedy: {}", wrap(&finding.remedy, 12));
        let _ = writeln!(out, "    costs:  {}", wrap(&finding.cost, 12));
        let _ = writeln!(out);
    }

    out
}

/// Renders the family yield table.
#[must_use]
pub fn render_yields(rows: &[Yield]) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "{:<20} {:>8} {:>10} {:>10} {:>16}", "family", "mutants", "cpu", "survivors", "survivors/cpu-h");

    for row in rows {
        let _ = writeln!(
            out,
            "{:<20} {:>8} {:>10} {:>10} {:>16.1}",
            row.family,
            row.mutants,
            human(row.cpu),
            row.survivors,
            row.per_cpu_hour()
        );
    }

    out
}

/// Where the rendered Markdown is going to be read.
///
/// The two destinations want genuinely different documents, not the same one at two sizes. A file
/// is opened on purpose by someone who wants the whole picture and needs to navigate it; a job
/// summary panel is scrolled past by someone who did not ask for it, sits under a heading the CI
/// renderer already owns, and has just been told the score and the verdict counts by the panel
/// above it. Repeating that there would be noise, and a level-one title nested under it would be
/// malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// A standalone file: title, table of contents, and what the run cost.
    #[default]
    Document,

    /// A fragment appended to something that already has a heading and a score.
    Embedded,
}

impl Layout {
    /// The heading prefix for a top-level section.
    const fn section(self) -> &'static str {
        match self {
            Self::Document => "##",
            Self::Embedded => "###",
        }
    }

    /// The heading prefix for one finding.
    const fn subsection(self) -> &'static str {
        match self {
            Self::Document => "###",
            Self::Embedded => "####",
        }
    }
}

/// Renders the diagnosis and the family table as a structured Markdown document.
///
/// The same analysis as the console rendering, in the one format that travels: a job summary panel,
/// a pull request comment, an issue, a file checked into a repository. Prose is left unwrapped
/// because a Markdown renderer reflows to the reader's width, and hard-wrapping to ours would
/// fight it.
///
/// The document is built to be navigated rather than read start to finish. A run that produced
/// eight findings is exactly the run whose reader wants to jump to one of them, so every section
/// is linkable, findings are numbered, and the table of contents names them.
#[must_use]
pub fn render_markdown(findings: &[Finding], rows: &[Yield], summary: Summary, timing: &Timing, layout: Layout) -> String {
    let mut out = String::new();

    if layout == Layout::Document {
        out.push_str("# Mutation testing advice\n\n");

        out.push_str(
            "What this run cost, what it found, and what could be changed — with the signal cost \
             of every change stated alongside it.\n\n",
        );

        write_contents(&mut out, findings, rows);
        write_outcome(&mut out, summary, timing);
    }

    write_findings(&mut out, findings, layout);
    write_yields(&mut out, rows, layout);
    write_glossary(&mut out, layout);

    out
}

/// Writes the table of contents.
fn write_contents(out: &mut String, findings: &[Finding], rows: &[Yield]) {
    out.push_str("## Contents\n\n");
    let _ = writeln!(out, "- [{RUN_HEADING}](#{})", slug(RUN_HEADING));
    let _ = writeln!(out, "- [{FINDINGS_HEADING}](#{})", slug(FINDINGS_HEADING));

    for (index, finding) in findings.iter().enumerate() {
        let heading = finding_heading(index, finding);

        let _ = writeln!(out, "  - [{heading}](#{})", slug(&heading));
    }

    if !rows.is_empty() {
        let _ = writeln!(out, "- [{YIELD_HEADING}](#{})", slug(YIELD_HEADING));
    }

    let _ = writeln!(out, "- [{GLOSSARY_HEADING}](#{})\n", slug(GLOSSARY_HEADING));
}

/// Writes what the run cost and what it decided, as two tables.
///
/// The verdicts and the cost are separate tables because they answer separate questions, and a
/// single table mixing counts with durations invites reading down a column that means two things.
fn write_outcome(out: &mut String, summary: Summary, timing: &Timing) {
    let _ = writeln!(out, "## {RUN_HEADING}\n");
    let _ = writeln!(out, "| Verdict | Mutants | Share of score |\n|---|---:|---:|");

    let valid = f64::from(summary.valid().max(1));

    for (label, count, scored) in [
        ("Killed", summary.killed, true),
        ("Timed out", summary.timeout, true),
        ("Survived", summary.survived, true),
        ("Uncovered", summary.uncovered, true),
        ("Unviable", summary.unviable, false),
        ("Ignored", summary.ignored, false),
        ("Not run", summary.pending, false),
    ] {
        if count == 0 {
            continue;
        }

        let share = if scored {
            format!("{:.1}%", f64::from(count) * 100.0 / valid)
        } else {
            "not scored".to_owned()
        };

        let _ = writeln!(out, "| {label} | {count} | {share} |");
    }

    let _ = writeln!(out, "| **Score** | | **{:.1}%** |\n", summary.score());

    let executed = timing.wall.saturating_sub(timing.build + timing.baseline);

    let _ = writeln!(out, "| Cost | Time | Share of run |\n|---|---:|---:|");

    for (label, spent) in [
        ("Build", timing.build),
        ("Baseline", timing.baseline),
        ("Testing mutants", executed),
    ] {
        let _ = writeln!(out, "| {label} | {} | {} |", human(spent), share(spent, timing.wall));
    }

    let _ = writeln!(out, "| **Total** | **{}** | at {} jobs |\n", human(timing.wall), timing.jobs);
}

/// Writes the findings, numbered so they can be referred to by position.
fn write_findings(out: &mut String, findings: &[Finding], layout: Layout) {
    let _ = writeln!(out, "{} {FINDINGS_HEADING}\n", layout.section());

    if findings.is_empty() {
        out.push_str(
            "Nothing crossed its threshold. Every check this tool makes looks for a cost that is \
             large enough to be worth trading signal for, and none of them fired.\n\n",
        );

        return;
    }

    out.push_str(
        "Findings are ordered so that the ones whose remedy costs no signal come first. A reader \
         who stops partway through stops having seen the free wins rather than the expensive \
         ones.\n\n",
    );

    for (index, finding) in findings.iter().enumerate() {
        let _ = writeln!(out, "{} {}\n", layout.subsection(), finding_heading(index, finding));
        let _ = writeln!(out, "Finding code: `{}`\n", finding.code);

        if !finding.detail.is_empty() {
            out.push_str("What was measured:\n\n");

            for line in &finding.detail {
                let _ = writeln!(out, "- {line}");
            }

            out.push('\n');
        }

        let _ = writeln!(out, "> **Remedy.** {}\n>", sentence(&finding.remedy));

        // The cost is never dropped, even here. A remedy quoted without what it gives up is how a
        // team ends up raising a score by measuring less.
        let _ = writeln!(out, "> **Costs.** {}\n", sentence(&finding.cost));
    }
}

/// Writes the per-family cost and value table.
fn write_yields(out: &mut String, rows: &[Yield], layout: Layout) {
    if rows.is_empty() {
        return;
    }

    let _ = writeln!(out, "{} {YIELD_HEADING}\n", layout.section());

    out.push_str(
        "Survivors per CPU-hour is what makes families comparable: it is the rate at which a \
         family bought the only thing a mutation run produces. A family near the bottom of this \
         table is the cheapest thing to turn off, and the last column says what turning it off \
         would have cost this run.\n\n",
    );

    out.push_str("| Family | Mutants | CPU | Survivors | Survivors/CPU-h |\n|---|---:|---:|---:|---:|\n");

    for row in rows {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {:.1} |",
            row.family,
            row.mutants,
            human(row.cpu),
            row.survivors,
            row.per_cpu_hour()
        );
    }

    out.push('\n');
}

/// Writes the definitions the rest of the document leans on.
///
/// Included because this file is written to be shared, and the person it gets forwarded to is
/// usually not the person who ran the tool.
fn write_glossary(out: &mut String, layout: Layout) {
    let _ = writeln!(out, "{} {GLOSSARY_HEADING}\n", layout.section());

    for (term, meaning) in [
        ("Killed", "a test failed while the mutant was active, which is the outcome you want."),
        ("Survived", "every test still passed with the mutant active, so nothing asserted on the behaviour it changed."),
        ("Timed out", "the suite never finished with the mutant active. Counted as detected: the change did not go unnoticed."),
        ("Uncovered", "no test reaches the code at all. Counted against the score exactly as a survivor is."),
        ("Unviable", "the mutant did not compile, so it says nothing about the tests and is left out of the score."),
        ("Baseline", "how long the suite takes with no mutant active. Every mutant pays this, so it multiplies by the population."),
    ] {
        let _ = writeln!(out, "- **{term}** — {meaning}");
    }

    out.push('\n');
}

/// The heading for one finding, numbered by position.
fn finding_heading(index: usize, finding: &Finding) -> String {
    format!("{}. {}", index + 1, sentence(&finding.headline))
}

/// A GitHub-style anchor for a heading, so the table of contents actually resolves.
///
/// Matches the rule GitHub, GitLab and most static site generators share: lowercase, drop anything
/// that is not a letter, a digit, a space or a hyphen, then turn spaces into hyphens.
fn slug(heading: &str) -> String {
    let mut anchor = String::with_capacity(heading.len());

    for character in heading.chars() {
        if character.is_alphanumeric() {
            anchor.extend(character.to_lowercase());
        } else if character == ' ' || character == '-' {
            anchor.push('-');
        }
    }

    anchor
}

/// One duration as a percentage of another, for a table column.
fn share(part: Duration, whole: Duration) -> String {
    if whole.is_zero() {
        return "—".to_owned();
    }

    format!("{:.0}%", part.as_secs_f64() * 100.0 / whole.as_secs_f64())
}

/// Capitalizes the first letter, for prose written to follow a lowercase console label.
///
/// The findings phrase themselves to sit after `remedy:` and `costs:`, which reads correctly there
/// and like a typo after a bold Markdown heading.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();

    chars.next().map_or_else(String::new, |first| first.to_uppercase().collect::<String>() + chars.as_str())
}

/// Wraps prose to a readable width, indenting continuation lines to `indent`.
fn wrap(text: &str, indent: usize) -> String {
    const WIDTH: usize = 84;

    let mut out = String::new();
    let mut column = indent;

    for word in text.split_whitespace() {
        if column + word.len() + 1 > WIDTH && column > indent {
            let _ = write!(out, "\n{:indent$}", "");
            column = indent;
        } else if column > indent {
            out.push(' ');
            column += 1;
        }

        out.push_str(word);
        column += word.len();
    }

    out
}

/// Renders a duration the way a person would say it.
#[must_use]
pub fn human(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();

    if seconds < 1.0 {
        return format!("{}ms", duration.as_millis());
    }

    if seconds < 90.0 {
        return format!("{seconds:.1}s");
    }

    let minutes = seconds / 60.0;

    if minutes < 90.0 {
        return format!("{minutes:.0}m");
    }

    format!("{:.1}h", minutes / 60.0)
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::ops::collect::Shape;

    fn mutant(file: &str, mutator: &str, outcome: Outcome, ms: u64) -> Mutant {
        Mutant {
            id: format!("{file}{mutator}{ms}"),
            ordinal: 1,
            package: "p".to_owned(),
            mutator: mutator.to_owned(),
            file: Utf8PathBuf::from(file),
            line: 1,
            column: 1,
            span: 0..1,
            item_path: "f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a".to_owned(),
            replacement: "b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: ms,
            killed_by: None,
            note: None,
        }
    }

    fn timing(build: u64, baseline: u64, wall: u64) -> Timing {
        Timing {
            build: Duration::from_secs(build),
            baseline: Duration::from_secs(baseline),
            wall: Duration::from_secs(wall),
            jobs: 4,
        }
    }

    fn find<'a>(findings: &'a [Finding], code: &str) -> Option<&'a Finding> {
        findings.iter().find(|finding| finding.code == code)
    }

    #[test]
    fn a_healthy_run_produces_no_findings() {
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100),
            mutant("b.rs", "arith.add_to_sub", Outcome::Survived, 100),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_dominant_build_is_reported() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let findings = analyze(&mutants, &timing(50, 5, 100));
        let finding = find(&findings, "fixed-cost").expect("expected a fixed-cost finding");

        assert!(finding.headline.contains("55%"), "{}", finding.headline);
    }

    #[test]
    fn a_build_that_is_a_small_part_of_the_run_is_not_reported() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let findings = analyze(&mutants, &timing(5, 1, 100));

        assert!(find(&findings, "fixed-cost").is_none(), "{findings:?}");
    }

    #[test]
    fn a_slow_baseline_projects_the_floor_of_the_run() {
        let mutants: Vec<Mutant> = (0..40)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        let findings = analyze(&mutants, &timing(1, 60, 4000));
        let finding = find(&findings, "slow-baseline").expect("expected a slow-baseline finding");

        // 40 mutants x 60s / 4 jobs = 600s.
        assert!(finding.detail.iter().any(|line| line.contains("10m")), "{finding:?}");
    }

    #[test]
    fn a_long_run_suggests_a_rotation_sized_to_it() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let findings = analyze(&mutants, &timing(1, 1, 3600));
        let finding = find(&findings, "long-run").expect("expected a long-run finding");

        // An hour at fifteen minutes a shard is four shards.
        assert!(finding.remedy.contains("--shard-count 4"), "{}", finding.remedy);
    }

    #[test]
    fn timeouts_report_the_budget_they_burned() {
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::Timeout, 30_000),
            mutant("a.rs", "relational.le_to_lt", Outcome::Timeout, 30_000),
            mutant("a.rs", "arith.add_to_sub", Outcome::Killed, 10),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "timeouts").expect("expected a timeouts finding");

        assert!(finding.headline.contains('2'), "{}", finding.headline);
        assert!(finding.detail[0].contains("60"), "{:?}", finding.detail);
    }

    #[test]
    fn the_timeout_remedy_names_its_cost() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Timeout, 10)];
        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "timeouts").expect("expected a timeouts finding");

        assert!(finding.cost.contains("goes unnoticed"), "{}", finding.cost);
    }

    #[test]
    fn a_pile_of_unviable_mutants_is_reported() {
        let mut mutants: Vec<Mutant> = (0..90)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend(
            (0..10).map(|index| mutant("b.rs", "fn_value.default", Outcome::CompileError, index)),
        );

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "unviable").expect("expected an unviable finding");

        assert!(finding.headline.contains("10 of 100"), "{}", finding.headline);
    }

    #[test]
    fn a_hot_file_names_the_survivors_that_would_be_lost() {
        let mut mutants: Vec<Mutant> = (0..80)
            .map(|index| mutant("generated.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.push(mutant("generated.rs", "arith.add_to_sub", Outcome::Survived, 1));
        mutants.extend(
            (0..19).map(|index| mutant("real.rs", "relational.lt_to_le", Outcome::Killed, index)),
        );

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "hot-file").expect("expected a hot-file finding");

        assert!(finding.headline.starts_with("generated.rs"), "{}", finding.headline);
        assert!(finding.cost.contains("81 mutants"), "{}", finding.cost);
        assert!(finding.cost.contains("1 of which"), "{}", finding.cost);
    }

    #[test]
    fn a_file_below_the_share_is_not_named() {
        let mut mutants: Vec<Mutant> = (0..95)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..5).map(|index| mutant("b.rs", "arith.add_to_sub", Outcome::Killed, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let hot: Vec<&Finding> = findings.iter().filter(|finding| finding.code == "hot-file").collect();

        assert_eq!(hot.len(), 1, "{hot:?}");
        assert!(hot[0].headline.starts_with("a.rs"), "{}", hot[0].headline);
    }

    #[test]
    fn a_family_that_finds_nothing_expensively_is_reported() {
        let mutants = vec![
            mutant("a.rs", "literal.int_bump", Outcome::Killed, 200_000),
            mutant("a.rs", "relational.lt_to_le", Outcome::Survived, 1000),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "low-yield").expect("expected a low-yield finding");

        assert!(finding.headline.contains("literal"), "{}", finding.headline);
        assert!(finding.remedy.contains("!literal"), "{}", finding.remedy);
    }

    #[test]
    fn a_family_that_finds_nothing_cheaply_is_left_alone() {
        let mutants = vec![
            mutant("a.rs", "literal.int_bump", Outcome::Killed, 10),
            mutant("a.rs", "relational.lt_to_le", Outcome::Survived, 50_000),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "low-yield").is_none(), "{findings:?}");
    }

    #[test]
    fn a_family_with_survivors_is_never_low_yield() {
        let mutants = vec![mutant("a.rs", "literal.int_bump", Outcome::Survived, 50_000)];
        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "low-yield").is_none(), "{findings:?}");
    }

    #[test]
    fn uncovered_code_is_reported_as_the_finding_it_is() {
        let mut mutants: Vec<Mutant> = (0..80)
            .map(|index| mutant("a.rs", "relational.lt_to_le", Outcome::Killed, index))
            .collect();

        mutants.extend((0..20).map(|index| mutant("b.rs", "arith.add_to_sub", Outcome::NoCoverage, index)));

        let findings = analyze(&mutants, &timing(1, 1, 100));
        let finding = find(&findings, "uncovered").expect("expected an uncovered finding");

        assert!(finding.headline.contains("20 of 100"), "{}", finding.headline);
        assert!(finding.remedy.contains("Write tests"), "{}", finding.remedy);
    }

    #[test]
    fn yields_rank_families_by_survivors_per_cpu_hour() {
        let mutants = vec![
            mutant("a.rs", "stmt.delete", Outcome::Survived, 1000),
            mutant("a.rs", "literal.int_bump", Outcome::Survived, 100_000),
        ];

        let rows = yields(&mutants);

        assert_eq!(rows[0].family, "stmt");
        assert_eq!(rows[1].family, "literal");
        assert!(rows[0].per_cpu_hour() > rows[1].per_cpu_hour());
    }

    #[test]
    fn a_mutator_without_a_family_is_its_own_family() {
        assert_eq!(family_of("relational.lt_to_le"), "relational");
        assert_eq!(family_of("odd"), "odd");
    }

    #[test]
    fn rendering_nothing_says_so_rather_than_printing_an_empty_page() {
        let rendered = render(&[]);

        assert!(rendered.contains("nothing to advise"), "{rendered}");
    }

    #[test]
    fn the_plain_yield_table_includes_the_family_rate() {
        let rows = vec![Yield {
            family: "literal".to_owned(),
            mutants: 2,
            cpu: Duration::from_secs(30),
            survivors: 1,
        }];

        let rendered = render_yields(&rows);

        // The plain text renderer is used outside Markdown contexts, where the per-hour yield is
        // the only column that makes families directly comparable.
        assert!(rendered.contains("family"), "{rendered}");
        assert!(rendered.contains("literal"), "{rendered}");
        assert!(rendered.contains("120.0"), "{rendered}");
    }

    #[test]
    fn every_rendered_finding_carries_its_cost() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Timeout, 10)];
        let findings = analyze(&mutants, &timing(50, 5, 100));
        let rendered = render(&findings);

        assert_eq!(rendered.matches("costs:").count(), findings.len(), "{rendered}");
    }

    #[test]
    fn a_tiny_run_is_never_diagnosed_by_share() {
        // Every file in a two-mutant run is half the population, which is arithmetic, not evidence.
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::NoCoverage, 100),
            mutant("b.rs", "arith.add_to_sub", Outcome::CompileError, 100),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "hot-file").is_none(), "{findings:?}");
        assert!(find(&findings, "unviable").is_none(), "{findings:?}");
        assert!(find(&findings, "uncovered").is_none(), "{findings:?}");
    }

    #[test]
    fn a_family_dominating_a_few_seconds_is_not_a_finding() {
        // 90% of six seconds is not worth anybody's attention.
        let mutants = vec![
            mutant("a.rs", "literal.int_bump", Outcome::Killed, 5000),
            mutant("a.rs", "relational.lt_to_le", Outcome::Survived, 500),
        ];

        let findings = analyze(&mutants, &timing(1, 1, 100));

        assert!(find(&findings, "low-yield").is_none(), "{findings:?}");
    }

    #[test]
    fn counts_of_one_are_not_pluralized() {
        assert_eq!(plural(1, "mutant"), "mutant");
        assert_eq!(plural(0, "mutant"), "mutants");
        assert_eq!(plural(2, "mutant"), "mutants");
    }

    #[test]
    fn durations_read_the_way_a_person_says_them() {
        assert_eq!(human(Duration::from_millis(250)), "250ms");
        assert_eq!(human(Duration::from_secs(9)), "9.0s");
        assert_eq!(human(Duration::from_secs(600)), "10m");
        assert_eq!(human(Duration::from_secs(7200)), "2.0h");
    }

    #[test]
    fn wrapping_indents_continuation_lines() {
        let text = "a ".repeat(80);
        let wrapped = wrap(&text, 4);

        assert!(wrapped.contains("\n    a"), "{wrapped}");
    }
    #[test]
    fn every_contents_entry_points_at_a_heading_that_exists() {
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100),
            mutant("a.rs", "arith.add_to_sub", Outcome::Survived, 100),
        ];

        let timing = timing(50, 5, 100);
        let findings = analyze(&mutants, &timing);
        let summary = Summary::of(&mutants);
        let document = render_markdown(&findings, &yields(&mutants), summary, &timing, Layout::Document);

        assert!(!findings.is_empty(), "the fixture must produce something to link to");

        // A table of contents whose links do not resolve is worse than none, because it is only
        // discovered to be broken by someone who already had to scroll.
        let anchors: Vec<String> = document
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("- [").or_else(|| line.trim_start().strip_prefix("  - [")))
            .filter_map(|entry| entry.split("](#").nth(1).map(|tail| tail.trim_end_matches(')').to_owned()))
            .collect();

        let headings: Vec<String> = document
            .lines()
            .filter_map(|line| line.strip_prefix("## ").or_else(|| line.strip_prefix("### ")))
            .map(slug)
            .collect();

        assert!(anchors.len() >= 4, "{document}");

        for anchor in anchors {
            assert!(headings.contains(&anchor), "`{anchor}` is not a heading in:\n{document}");
        }
    }

    #[test]
    fn a_run_with_nothing_to_report_still_says_what_it_cost() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let timing = timing(1, 1, 100);
        let document = render_markdown(&[], &yields(&mutants), Summary::of(&mutants), &timing, Layout::Document);

        assert!(document.contains("## This run"), "{document}");
        assert!(document.contains("Nothing crossed its threshold"), "{document}");
        assert!(document.contains("| **Score** | | **100.0%** |"), "{document}");
    }

    #[test]
    fn unscored_outcomes_are_named_as_not_scored_in_the_run_table() {
        let summary = Summary {
            killed: 1,
            survived: 0,
            timeout: 0,
            out_of_memory: 0,
            unviable: 1,
            ignored: 1,
            uncovered: 0,
            not_built: 0,
            pending: 1,
        };
        let timing = timing(1, 1, 10);
        let document = render_markdown(&[], &[], summary, &timing, Layout::Document);

        // Unviable, ignored and pending mutants are not in the denominator, so the table must not
        // present them as a share of the mutation score.
        assert_eq!(document.matches("not scored").count(), 3, "{document}");
    }

    #[test]
    fn a_slug_matches_the_anchor_a_markdown_renderer_would_generate() {
        assert_eq!(slug("Yield by mutator family"), "yield-by-mutator-family");
        assert_eq!(slug("1. 97% of the run was the build"), "1-97-of-the-run-was-the-build");
    }

    #[test]
    fn a_run_with_no_wall_time_reports_no_share_rather_than_a_division_by_zero() {
        assert_eq!(share(Duration::from_secs(1), Duration::ZERO), "—");
    }

    #[test]
    fn the_embedded_layout_nests_under_the_heading_its_host_already_wrote() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let timing = timing(50, 5, 100);
        let findings = analyze(&mutants, &timing);
        let summary = Summary::of(&mutants);
        let panel = render_markdown(&findings, &yields(&mutants), summary, &timing, Layout::Embedded);

        assert!(!panel.contains("# Mutation testing advice"), "{panel}");
        assert!(!panel.contains("## Contents"), "{panel}");

        // The job summary panel states the score and the verdict counts itself, directly above.
        assert!(!panel.contains(RUN_HEADING), "{panel}");
        assert!(panel.starts_with("### Findings"), "{panel}");
    }

}
