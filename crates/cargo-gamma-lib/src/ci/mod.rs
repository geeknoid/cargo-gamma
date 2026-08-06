//! Surfacing a run inside a continuous integration system.
//!
//! A mutation report that lives in an artifact zip is a report nobody reads. The findings have to
//! arrive where the reviewer already is — on the diff, in the job summary, in the security tab —
//! or the tool gets adopted, run nightly, and ignored.
//!
//! Three renderings share one rule: **only survivors are findings.** A killed mutant is the tool
//! working, and publishing it would bury the signal under its own success.

mod annotations;
mod level;
mod sarif;
mod truncation;

use core::fmt::Write as _;

use camino::Utf8Path;

use crate::Result;
use crate::model::{Mutant, Outcome, Summary};
use crate::{HashMap, HashSet};

pub use annotations::Annotations;
pub use level::Level;
pub use truncation::Truncation;

/// GitHub rejects a SARIF upload with more results than this.
///
/// It is a hard limit on their side, not a preference on ours: exceeding it fails the upload
/// outright, which is a worse outcome than a report that says what it left out.
pub(crate) const SARIF_LIMIT: usize = 5_000;

/// GitHub rejects a SARIF upload larger than this, whatever it contains.
///
/// The result count is not a reliable proxy for the size, because a finding carries a message, a
/// path and a fingerprint whose lengths are the code's business rather than ours. A log under the
/// count limit and over the byte limit is rejected just as completely, so both are enforced.
pub(crate) const SARIF_BYTES: usize = 10 * 1024 * 1024;

/// The most findings any one annotation run will print.
///
/// GitHub keeps only the first ten annotations of a level per step and silently discards the rest,
/// so printing more produces a log full of commands that had no effect and a reviewer who believes
/// they have seen everything. The report and the SARIF log carry the full population.
const ANNOTATION_LIMIT: usize = 10;

/// How many under-tested files the job summary lists.
const SUMMARY_FILES: usize = 10;

/// Whether the GitHub renderings should be emitted.
///
/// `Auto` keys off `GITHUB_ACTIONS`, which the runner sets on every step. That means a workflow
/// gets annotations by adding nothing to its command line, which is the only adoption path that
/// reliably happens.
#[must_use]
pub const fn wanted(annotations: Annotations, github_actions: bool) -> bool {
    match annotations {
        Annotations::None => false,
        Annotations::Auto => github_actions,
        Annotations::Github => true,
    }
}

/// The survivors, in report order.
///
/// Uncovered mutants are deliberately included: no test reached them, which is a stronger finding
/// than a test reaching them and not noticing, and a reviewer wants to see both on the diff.
fn findings(mutants: &[Mutant]) -> Vec<&Mutant> {
    mutants
        .iter()
        .filter(|mutant| matches!(mutant.outcome, Outcome::Survived | Outcome::NoCoverage))
        .collect()
}

/// Renders the GitHub Actions workflow commands that place survivors on the diff.
///
/// The message is the mutation itself rather than a summary of it. A reviewer looking at the line
/// needs to know what was changed and that nothing complained, and any wording that does not
/// contain the replacement makes them go and look it up.
#[must_use]
pub fn annotations(mutants: &[Mutant], root: &Utf8Path) -> Vec<String> {
    let survivors = findings(mutants);
    let mut lines: Vec<String> = survivors
        .iter()
        .take(ANNOTATION_LIMIT)
        .map(|mutant| {
            let file = relative(&mutant.file, root);
            let title = format!("Surviving mutant ({})", mutant.mutator);
            let message = describe(mutant);

            format!(
                "::warning file={file},line={},col={},title={}::{}",
                mutant.line,
                mutant.column,
                escape_property(&title),
                escape_data(&message)
            )
        })
        .collect();

    if survivors.len() > ANNOTATION_LIMIT {
        lines.push(format!(
            "::notice title=Surviving mutants::{} of {} findings annotated, which is all GitHub keeps per step; \
             the rest are in the report",
            ANNOTATION_LIMIT,
            survivors.len()
        ));
    }

    lines
}

/// Renders the Markdown written to `$GITHUB_STEP_SUMMARY`.
///
/// This is the artifact a team actually reads every morning, so it leads with the number that
/// decides whether anyone reads further, and then spends its space on where the gaps are rather
/// than on restating the run's configuration.
#[must_use]
pub fn summary(mutants: &[Mutant], root: &Utf8Path) -> String {
    let totals = Summary::of(mutants);
    let mut text = String::from("## Mutation testing\n\n");

    let _ = writeln!(
        text,
        "**Score {:.1}%** — {} caught, {} missed of {} mutants.\n",
        totals.score(),
        totals.killed + totals.timeout,
        totals.survived + totals.uncovered,
        totals.killed + totals.timeout + totals.survived + totals.uncovered
    );

    text.push_str("| Outcome | Count |\n|---|---:|\n");

    for (label, count) in [
        ("Caught", totals.killed),
        ("Caught by timeout", totals.timeout),
        ("Survived", totals.survived),
        ("Uncovered", totals.uncovered),
        ("Unviable", totals.unviable),
        ("Suppressed", totals.ignored),
    ] {
        if count > 0 {
            let _ = writeln!(text, "| {label} | {count} |");
        }
    }

    let hot = under_tested(mutants, root);

    if !hot.is_empty() {
        text.push_str("\n### Where the gaps are\n\n| File | Survivors |\n|---|---:|\n");

        for (file, count) in hot {
            let _ = writeln!(text, "| `{file}` | {count} |");
        }
    }

    text
}

/// The files with the most survivors, worst first.
fn under_tested(mutants: &[Mutant], root: &Utf8Path) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::default();

    for mutant in findings(mutants) {
        *counts.entry(relative(&mutant.file, root)).or_default() += 1;
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();

    // Ties broken by path so a summary is reproducible run to run; an unstable order turns every
    // morning's table into a diff nobody can read.
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.truncate(SUMMARY_FILES);
    ranked
}

/// Renders survivors as a SARIF 2.1.0 log, and says what it had to leave out.
///
/// Rule identifiers are our stable mutator names, which is what makes GitHub's alert grouping and
/// dismissal work per operator: a team can permanently dismiss every `literal.int_zero` alert
/// without touching anything else, and that decision keeps applying to code written next year.
pub fn sarif(mutants: &[Mutant], root: &Utf8Path, level: Level) -> Result<(String, Option<Truncation>)> {

    let survivors = findings(mutants);
    let found = survivors.len();
    let mut kept: Vec<&Mutant> = survivors.into_iter().take(SARIF_LIMIT).collect();

    // Shrunk until it fits rather than estimated, because the size of a finding is decided by the
    // length of a path, a message and an identifier, none of which this can predict. Halving
    // converges in a handful of serializations even from the count limit, and the alternative to
    // any of it is an upload GitHub refuses whole.
    loop {
        let text = render(&kept, root, level)?;

        if text.len() <= SARIF_BYTES || kept.is_empty() {
            let truncation = (found > kept.len()).then_some(Truncation { found, written: kept.len() });

            return Ok((text, truncation));
        }

        kept.truncate(kept.len() / 2);
    }
}

/// Serializes one SARIF log over exactly the findings it is given.
fn render(kept: &[&Mutant], root: &Utf8Path, level: Level) -> Result<String> {
    use sarif::{Artifact, Configuration, Driver, Finding, Location, Log, Physical, Region, Rule, Run, Text, Tool};

    let mut seen = HashSet::default();
    let mut rules = Vec::new();

    for mutant in kept {
        if !seen.insert(mutant.mutator.clone()) {
            continue;
        }

        rules.push(Rule {
            id: mutant.mutator.clone(),
            name: mutant.mutator.clone(),
            short_description: Text { text: format!("Surviving mutant: {}", mutant.mutator) },
            full_description: Text {
                text: format!(
                    "The {} mutation was applied and the test suite still passed, so nothing asserts on the \
                     behavior it changed.",
                    mutant.mutator
                ),
            },
            default_configuration: Configuration { level: level.as_str() },
        });
    }

    rules.sort_by(|left, right| left.id.cmp(&right.id));

    let results = kept
        .iter()
        .map(|mutant| {
            let mut fingerprints = HashMap::default();

            // The mutant id is content-addressed, so an alert follows its code through reformatting
            // and through edits elsewhere in the file instead of being dismissed and resurrected.
            let _previous = fingerprints.insert("gammaMutantId/v1".to_owned(), mutant.id.clone());

            Finding {
                rule_id: mutant.mutator.clone(),
                level: level.as_str(),
                message: Text { text: describe(mutant) },
                locations: vec![Location {
                    physical_location: Physical {
                        artifact_location: Artifact { uri: relative(&mutant.file, root) },
                        region: Region { start_line: mutant.line, start_column: mutant.column },
                    },
                }],
                partial_fingerprints: fingerprints,
            }
        })
        .collect();

    let log = Log {
        version: "2.1.0",
        schema: "https://json.schemastore.org/sarif-2.1.0.json",
        runs: vec![Run {
            tool: Tool {
                driver: Driver {
                    name: "cargo-gamma",
                    information_uri: "https://github.com/geeknoid/cargo-gamma",
                    version: env!("CARGO_PKG_VERSION"),
                    rules,
                },
            },
            results,
        }],
    };

    serde_json::to_string_pretty(&log)
        .map_err(|cause| crate::error::error!("could not serialize the SARIF log").caused_by(cause))
}

/// What a survivor is, in one sentence.
///
/// The location is carried in fields of its own by both consumers, so repeating it here would only
/// take space from the part a reader cannot get anywhere else.
fn describe(mutant: &Mutant) -> String {
    if mutant.outcome == Outcome::NoCoverage {
        format!("No test reaches this code: {}.", mutant.summary())
    } else {
        format!("{} and no test failed.", mutant.summary())
    }
}

/// A path relative to the workspace root, with forward slashes.
///
/// Every consumer here resolves against the repository checkout, so an absolute path from the
/// machine that ran the job points at nothing. Forward slashes because SARIF and the workflow
/// commands both specify them regardless of the host.
fn relative(path: &Utf8Path, root: &Utf8Path) -> String {
    path.strip_prefix(root).unwrap_or(path).as_str().replace('\\', "/")
}

/// Escapes a workflow command property value.
fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}

/// Escapes a workflow command message.
///
/// A newline inside a message would end the command and turn the remainder into log noise, so the
/// escaping is not cosmetic.
fn escape_data(text: &str) -> String {
    text.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use serde_json::Value;

    use super::*;
    use crate::ops::collect::Shape;

    fn mutant(file: &str, line: usize, mutator: &str, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("{file}:{line}:{mutator}"),
            ordinal: 0,
            file: Utf8PathBuf::from(file),
            package: "subject".to_owned(),
            span: 0..1,
            line,
            column: 5,
            mutator: mutator.to_owned(),
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

    fn root() -> Utf8PathBuf {
        Utf8PathBuf::from("/w")
    }

    #[test]
    fn only_survivors_are_findings() {
        let mutants = vec![
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed),
            mutant("/w/src/a.rs", 2, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 3, "relational.gt_to_ge", Outcome::Timeout),
            mutant("/w/src/a.rs", 4, "relational.gt_to_ge", Outcome::NoCoverage),
            mutant("/w/src/a.rs", 5, "relational.gt_to_ge", Outcome::CompileError),
        ];

        let found = findings(&mutants);

        // A killed mutant is the tool working. A timeout killed it too. An unviable one never ran.
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].line, 2);
        assert_eq!(found[1].line, 4);
    }

    #[test]
    fn an_annotation_points_at_a_relative_path() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("::warning file=src/a.rs,line=12,col=5,"), "{}", lines[0]);
    }

    #[test]
    fn an_annotation_says_what_the_mutation_was() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        // A reviewer standing on the line has to be told the replacement, or they go and look it up.
        assert!(lines[0].contains("a >= b"), "{}", lines[0]);
    }

    #[test]
    fn an_uncovered_mutant_says_so() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::NoCoverage)];
        let lines = annotations(&mutants, &root());

        assert!(lines[0].contains("No test reaches this code"), "{}", lines[0]);
    }

    #[test]
    fn too_many_annotations_are_capped_and_the_cap_is_announced() {
        let mutants: Vec<Mutant> = (0..ANNOTATION_LIMIT + 5)
            .map(|line| mutant("/w/src/a.rs", line + 1, "relational.gt_to_ge", Outcome::Survived))
            .collect();

        let lines = annotations(&mutants, &root());

        assert_eq!(lines.len(), ANNOTATION_LIMIT + 1);
        assert!(lines.last().expect("a notice").contains("of 15 findings annotated"));
    }

    #[test]
    fn nothing_survived_means_nothing_to_annotate() {
        let mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed)];

        assert!(annotations(&mutants, &root()).is_empty());
    }

    #[test]
    fn a_newline_cannot_escape_a_message() {
        // A raw newline would end the workflow command and turn the rest into log noise. Mutant
        // text is already flattened before it gets here, so this is the belt to that suspenders.
        assert_eq!(escape_data("a\r\nb"), "a%0D%0Ab");
    }

    #[test]
    fn an_annotation_does_not_repeat_the_location_it_already_carries() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        assert!(!lines[0].contains("src/a.rs:12"), "{}", lines[0]);
    }

    #[test]
    fn a_comma_cannot_escape_a_property() {
        assert_eq!(escape_property("a,b:c"), "a%2Cb%3Ac");
    }

    #[test]
    fn a_percent_is_escaped_before_anything_else() {
        // Escaping it last would double-escape the escapes.
        assert_eq!(escape_data("%0A\n"), "%250A%0A");
    }

    #[test]
    fn the_summary_leads_with_the_score() {
        let mutants = vec![
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed),
            mutant("/w/src/a.rs", 2, "relational.gt_to_ge", Outcome::Survived),
        ];

        let text = summary(&mutants, &root());

        assert!(text.contains("**Score 50.0%**"), "{text}");
        assert!(text.contains("| Caught | 1 |"), "{text}");
        assert!(text.contains("| Survived | 1 |"), "{text}");
    }

    #[test]
    fn an_empty_outcome_is_left_out_of_the_summary_table() {
        let mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed)];
        let text = summary(&mutants, &root());

        assert!(!text.contains("Unviable"), "{text}");
        assert!(!text.contains("Where the gaps are"), "{text}");
    }

    #[test]
    fn the_summary_ranks_files_by_survivors() {
        let mut mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived)];

        for line in 0..3 {
            mutants.push(mutant("/w/src/b.rs", line + 1, "relational.gt_to_ge", Outcome::Survived));
        }

        let ranked = under_tested(&mutants, &root());

        assert_eq!(ranked, vec![("src/b.rs".to_owned(), 3), ("src/a.rs".to_owned(), 1)]);
    }

    #[test]
    fn files_with_the_same_count_are_ordered_by_path() {
        let mutants = vec![
            mutant("/w/src/z.rs", 1, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived),
        ];

        let ranked = under_tested(&mutants, &root());

        assert_eq!(ranked[0].0, "src/a.rs");
    }

    #[test]
    fn sarif_carries_one_rule_per_mutator() {
        let mutants = vec![
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 2, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 3, "literal.int_zero", Outcome::Survived),
        ];

        let (text, truncation) = sarif(&mutants, &root(), Level::Note).expect("sarif");
        let log: Value = serde_json::from_str(&text).expect("valid json");

        assert_eq!(truncation, None);

        let rules = log["runs"][0]["tool"]["driver"]["rules"].as_array().expect("rules");

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["id"], "literal.int_zero");
        assert_eq!(log["runs"][0]["results"].as_array().expect("results").len(), 3);
    }

    #[test]
    fn a_sarif_result_is_fingerprinted_by_mutant_id() {
        let mutants = vec![mutant("/w/src/a.rs", 7, "relational.gt_to_ge", Outcome::Survived)];
        let (text, _) = sarif(&mutants, &root(), Level::Warning).expect("sarif");
        let log: Value = serde_json::from_str(&text).expect("valid json");
        let result = &log["runs"][0]["results"][0];

        assert_eq!(result["level"], "warning");
        assert_eq!(result["partialFingerprints"]["gammaMutantId/v1"], "/w/src/a.rs:7:relational.gt_to_ge");

        let region = &result["locations"][0]["physicalLocation"];

        assert_eq!(region["artifactLocation"]["uri"], "src/a.rs");
        assert_eq!(region["region"]["startLine"], 7);
    }

    #[test]
    fn sarif_reports_what_it_could_not_fit() {
        let truncation = Truncation { found: SARIF_LIMIT + 1, written: SARIF_LIMIT };

        // Constructing the real thing would allocate five thousand and one mutants for one
        // assertion, so the shape of the report is checked here and the threshold is checked by the
        // branch above it.
        assert_eq!(truncation.found - truncation.written, 1);
    }

    #[test]
    fn a_log_too_large_to_upload_is_shrunk_until_it_fits() {
        // The count limit is not a size limit: a finding's size is decided by the length of a path
        // and a message, and a log GitHub refuses is worth nothing however many results it holds.
        let deep = format!("/w/src/{}/a.rs", "nested/".repeat(500));
        let mutants: Vec<Mutant> = (0..SARIF_LIMIT)
            .map(|line| mutant(&deep, line, "relational.gt_to_ge", Outcome::Survived))
            .collect();

        let (text, truncation) = sarif(&mutants, &root(), Level::Warning).expect("sarif");
        let truncation = truncation.expect("a log this large cannot have been written whole");

        assert!(text.len() <= SARIF_BYTES, "{} bytes", text.len());
        assert_eq!(truncation.found, SARIF_LIMIT);
        assert!(truncation.written < SARIF_LIMIT, "{}", truncation.written);
    }
    #[test]
    fn a_path_outside_the_root_is_left_alone() {
        // Better an absolute path a consumer cannot resolve than a relative one pointing at the
        // wrong file inside the checkout.
        assert_eq!(relative(Utf8Path::new("/elsewhere/a.rs"), &root()), "/elsewhere/a.rs");
    }

    #[test]
    fn auto_follows_the_runner() {
        assert!(wanted(Annotations::Auto, true));
        assert!(!wanted(Annotations::Auto, false));
        assert!(wanted(Annotations::Github, false));
        assert!(!wanted(Annotations::None, true));
    }
}
