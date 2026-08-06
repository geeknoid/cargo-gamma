//! The `mutation-testing-elements` report schema.
//!
//! This is the interchange format the Stryker report viewers consume, and emitting it is what
//! gives cargo-gamma a report UI, an Azure DevOps extension and a GitHub integration without
//! writing any of them. The schema is a published artifact of another project, so the mapping is
//! spelled out here rather than left implicit: drift is silent and shows up as a blank page in
//! someone's browser rather than as a failing build.

use core::fmt::Write as _;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::HashMap;
use crate::Result;
use crate::discover::Plan;
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::parse::SourceFile;

/// The schema version we emit.
///
/// The version string validates against `^([1-2])(\.(([1-9]\d*)|0)){0,2}$` — major 1 and 2 only —
/// even though the npm package that defines it is at 3.x. Emitting "3" fails validation for a
/// reason that looks like version skew and is not.
const SCHEMA_VERSION: &str = "2";

/// A whole mutation test result, the root of the report document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// The schema version this document claims to conform to.
    pub schema_version: String,

    /// The score bands the viewer colors by.
    pub thresholds: Thresholds,

    /// Absolute path the file keys are relative to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,

    /// What produced the report.
    pub framework: Framework,

    /// One entry per mutated file, keyed by workspace-relative path.
    pub files: HashMap<String, FileResult>,

    /// Free-form run metadata.
    ///
    /// The schema declares this "free-format", which is what makes it the right home for the shard
    /// identity and the run time. `merge` needs both, and inventing a sidecar file for them would
    /// mean a report artifact that is only half the story.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<RunInfo>,
}

/// What `merge` needs to know about the run that produced a report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInfo {
    /// When the run started, in seconds since the Unix epoch.
    ///
    /// Seconds rather than a formatted timestamp because every use is arithmetic — freshness,
    /// ordering, windowing — and parsing a date format back is a step that can only lose.
    pub started_at: u64,

    /// The shard this run covered, when it was sharded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<ShardInfo>,
}

/// Identifies one shard of a rotation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardInfo {
    /// Which shard this was, from zero.
    pub index: u32,

    /// How many shards the population was divided into.
    pub count: u32,
}

/// The score bands the viewer colors by.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    /// At or above this score the viewer shows green.
    pub high: u32,

    /// Below this score the viewer shows red.
    pub low: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { high: 80, low: 60 }
    }
}

/// Identifies the tool, which the viewer shows in its header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Framework {
    /// The tool name.
    pub name: String,

    /// The tool version.
    pub version: String,
}

/// One mutated file: its full source, and every mutant generated in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResult {
    /// The complete source text. The viewer renders this with the mutants overlaid, which is why
    /// the report is self-contained and can be opened without the repository.
    pub source: String,

    /// The language, used to pick syntax highlighting.
    pub language: String,

    /// Every mutant in this file.
    pub mutants: Vec<MutantResult>,
}

/// One mutant, in the schema's vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutantResult {
    /// Our content-addressed identity.
    ///
    /// Stryker uses a within-run integer here; the field is a free-form string, and an identity
    /// that survives edits elsewhere in the file is strictly more useful in a report someone may
    /// compare against last week's.
    pub id: String,

    /// The registry name of the mutator, such as `relational.lt_to_le`.
    ///
    /// The viewer groups and filters by this string, so the naming scheme becomes the UI's facet
    /// list at no extra cost.
    pub mutator_name: String,

    /// Where the mutated construct is.
    pub location: Location,

    /// The verdict, in the schema's closed `PascalCase` vocabulary.
    ///
    /// Owned rather than `&'static str` so a written report can be read back — which is what
    /// `merge` does. The closed-enum guarantee is kept at the only place it can be violated, the
    /// mapping in `status_of`, and asserted against the vendored schema by a conformance test.
    pub status: String,

    /// The replacement source text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,

    /// A human sentence describing the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Why the status is what it is: a suppression reason, or the test that killed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,

    /// Wall time spent on this mutant, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,

    /// The test that killed it, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<Vec<String>>,
}

/// A half-open source range, in the schema's one-based line and column terms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Location {
    /// Inclusive start.
    pub start: Position,

    /// Exclusive end.
    pub end: Position,
}

/// A one-based line and column.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    /// One-based line.
    pub line: usize,

    /// One-based column.
    pub column: usize,
}

/// Maps a verdict onto the schema's closed status enum.
///
/// The enum has no room for invention, and three of these are worth stating because they are not
/// obvious. `Timeout` counts as *detected* in the schema's own metric, which is the same position
/// we take — a hanging mutant was caught, it just cost wall time — so the score the viewer computes
/// and the score we print agree by construction. A suppressed mutant becomes `Ignored`, which the
/// schema also excludes from the denominator.
///
/// A mutant stopped by its memory ceiling is reported as `Timeout`, which is the closest the schema
/// offers: it is the only other resource-exhaustion verdict, and the only one the schema counts as
/// detected. `RuntimeError` is the superficially better name and the wrong answer, because the
/// schema excludes it from the denominator, so exporting it there would silently lower the score a
/// reader sees in the viewer relative to the one printed here.
const fn status_of(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pending => "Pending",
        Outcome::Killed => "Killed",
        Outcome::Survived => "Survived",
        Outcome::Timeout | Outcome::OutOfMemory => "Timeout",
        Outcome::CompileError => "CompileError",
        // A mutant the build never compiled is `Ignored` rather than `NoCoverage`: both are real
        // options here, but `NoCoverage` is in the schema's denominator and would lower the score
        // the viewer shows relative to the printed one, which is the disagreement this whole
        // outcome exists to remove.
        Outcome::Ignored | Outcome::NotBuilt => "Ignored",
        Outcome::NoCoverage => "NoCoverage",
    }
}

/// Explains a verdict in one sentence, for the viewer's detail pane.
fn reason_for(mutant: &Mutant) -> Option<String> {
    if let Some(suppression) = mutant.suppression.as_ref() {
        let mut text = format!("suppressed by {}", suppression.channel.as_str());

        if let Some(reason) = suppression.reason.as_ref() {
            text.push_str(": ");
            text.push_str(reason);
        }

        if let Some(tag) = suppression.tag.as_ref() {
            let _ = write!(text, " [#{tag}]");
        }

        return Some(text);
    }

    match mutant.outcome {
        Outcome::Killed => mutant.killed_by.as_ref().map(|test| format!("failed `{test}`")),
        Outcome::CompileError => Some("the mutant does not compile".to_owned()),
        Outcome::Timeout => Some(
            mutant
                .note
                .clone()
                .unwrap_or_else(|| "the test run exceeded its budget".to_owned()),
        ),
        _ => None,
    }
}

/// Builds the report document for a completed plan.
///
/// Every mutated file's full source is embedded, because a report that needs the repository beside
/// it to be readable cannot be attached to a CI run or mailed to someone.
pub fn build(plan: &Plan, thresholds: Thresholds, run: Option<RunInfo>) -> Result<Report> {
    let mut files: HashMap<String, FileResult> = HashMap::default();

    // Grouped once rather than rescanned per file: a workspace with many files has many mutants
    // too, so the pairing is quadratic in exactly the case it needs not to be.
    let mut grouped: HashMap<&Utf8Path, Vec<&Mutant>> = HashMap::default();

    for mutant in &plan.mutants {
        grouped.entry(mutant.file.as_path()).or_default().push(mutant);
    }

    for file in &plan.files {
        let Some(mutants) = grouped.get(file.path.as_path()) else {
            continue;
        };

        let source = SourceFile::read(&file.absolute)?;
        let rendered = mutants.iter().map(|mutant| render(mutant, &source)).collect();

        let _ = files.insert(
            file.path.to_string(),
            FileResult {
                source: source.text.clone(),
                language: "rust".to_owned(),
                mutants: rendered,
            },
        );
    }

    Ok(Report {
        schema_version: SCHEMA_VERSION.to_owned(),
        thresholds,
        project_root: Some(plan.root.to_string()),
        framework: Framework {
            name: "cargo-gamma".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        files,
        config: run,
    })
}

/// Converts one mutant into its schema form.
fn render(mutant: &Mutant, source: &SourceFile) -> MutantResult {
    let (start_line, start_column) = source.location(mutant.span.start);
    let (end_line, end_column) = source.location(mutant.span.end);

    MutantResult {
        id: mutant.id.clone(),
        mutator_name: mutant.mutator.clone(),
        location: Location {
            start: Position {
                line: start_line,
                column: start_column,
            },
            end: Position {
                line: end_line,
                column: end_column,
            },
        },
        status: status_of(mutant.outcome).to_owned(),
        replacement: Some(mutant.replacement.clone()),
        description: Some(mutant.summary()),
        status_reason: reason_for(mutant),
        duration: (mutant.elapsed_ms > 0).then_some(mutant.elapsed_ms),
        killed_by: mutant.killed_by.clone().map(|test| vec![test]),
    }
}

/// Reads the ids of mutants an earlier report already settled.
///
/// A settled mutant is one a rerun would learn nothing from: it was killed, it timed out (which
/// counts as detected), it could not compile, or it was suppressed. Survivors and mutants nothing
/// covered are deliberately left out, because those are the ones a later run is meant to revisit.
///
/// # Errors
///
/// Returns an error if the text is not a report this tool wrote.
pub fn settled_mutants(text: &str) -> Result<crate::HashSet<String>, String> {
    let report: Report = serde_json::from_str(text).map_err(|cause| cause.to_string())?;

    Ok(report
        .files
        .values()
        .flat_map(|file| file.mutants.iter())
        .filter(|mutant| is_settled(&mutant.status))
        .map(|mutant| mutant.id.clone())
        .collect())
}

/// Returns whether a recorded status is one a rerun would not change.
fn is_settled(status: &str) -> bool {
    matches!(status, "Killed" | "Timeout" | "CompileError" | "Ignored")
}

/// Serializes the report as pretty-printed JSON.
pub fn to_json(report: &Report) -> Result<String> {
    serde_json::to_string_pretty(report).map_err(|cause| error!("could not serialize the report").caused_by(cause))
}

/// Writes the report to a path, creating parent directories as needed.
pub fn write(path: &Utf8PathBuf, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
    }

    fs::write(path.as_std_path(), contents).map_err(|cause| error!("could not write `{path}`").caused_by(cause))
}
#[cfg(test)]
mod tests {
    use core::ops::Range;

    use super::*;
    use crate::discover::TargetFile;
    use crate::model::{Channel, Suppression};
    use crate::ops::collect::Shape;

    fn mutant(outcome: Outcome, span: Range<usize>) -> Mutant {
        Mutant {
            id: "abc123abc123".to_owned(),
            ordinal: 1,
            file: Utf8PathBuf::from("src/lib.rs"),
            package: "subject".to_owned(),
            span,
            line: 1,
            column: 1,
            mutator: "relational.lt_to_le".to_owned(),
            item_path: "f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a < b".to_owned(),
            replacement: "(a) <= (b)".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    #[test]
    fn every_verdict_maps_onto_the_closed_enum() {
        // The schema's status list is closed, so a verdict we invent a name for would fail
        // validation in the viewer rather than here.
        const VALID: [&str; 7] = [
            "Killed",
            "Survived",
            "NoCoverage",
            "CompileError",
            "RuntimeError",
            "Timeout",
            "Ignored",
        ];

        for outcome in [
            Outcome::Killed,
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::CompileError,
            Outcome::Ignored,
            Outcome::NoCoverage,
        ] {
            assert!(VALID.contains(&status_of(outcome)), "{outcome} maps outside the enum");
        }

        assert_eq!(status_of(Outcome::Pending), "Pending");
    }

    #[test]
    fn the_schema_version_is_in_the_supported_range() {
        // The npm package is at 3.x but the schema only validates major 1 and 2.
        assert_eq!(SCHEMA_VERSION, "2");
    }

    #[test]
    fn a_span_becomes_a_one_based_half_open_location() {
        let source = SourceFile::parse("src/lib.rs", "fn f() {\n    a < b\n}\n".to_owned()).expect("parses");
        let start = source.text.find("a <").expect("present");
        let rendered = render(&mutant(Outcome::Survived, start..start + 5), &source);

        assert_eq!(rendered.location.start.line, 2);
        assert_eq!(rendered.location.start.column, 5);
        assert_eq!(rendered.location.end.line, 2);
        assert_eq!(rendered.location.end.column, 10);
    }

    #[test]
    fn a_killing_test_is_named_in_the_status_reason() {
        let mut subject = mutant(Outcome::Killed, 0..1);

        subject.killed_by = Some("tests::the_boundary".to_owned());

        assert_eq!(reason_for(&subject), Some("failed `tests::the_boundary`".to_owned()));
        assert_eq!(
            render(
                &subject,
                &SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses")
            )
            .killed_by,
            Some(vec!["tests::the_boundary".to_owned()])
        );
    }

    #[test]
    fn unviable_and_timeout_mutants_explain_their_status() {
        let mut timed_out = mutant(Outcome::Timeout, 0..1);

        // These verdicts are not self-explanatory in the report viewer, so the reason field
        // distinguishes a compile failure from a budget overrun.
        assert_eq!(
            reason_for(&mutant(Outcome::CompileError, 0..1)),
            Some("the mutant does not compile".to_owned())
        );
        assert_eq!(
            reason_for(&timed_out),
            Some("the test run exceeded its budget".to_owned())
        );

        timed_out.note = Some("stalled, last test named was `slow_case`".to_owned());

        assert_eq!(reason_for(&timed_out), Some("stalled, last test named was `slow_case`".to_owned()));
    }

    #[test]
    fn a_suppression_carries_its_reason_and_tag_into_the_report() {
        // This is what makes suppressions auditable at a glance in the viewer, rather than a
        // silent hole in the population.
        let mut subject = mutant(Outcome::Ignored, 0..1);

        subject.suppression = Some(Suppression {
            channel: Channel::Comment,
            reason: Some("fixed-point math".to_owned()),
            tag: Some("perf".to_owned()),
            line: Some(4),
        });

        assert_eq!(
            reason_for(&subject),
            Some("suppressed by comment: fixed-point math [#perf]".to_owned())
        );
    }

    #[test]
    fn an_untimed_mutant_omits_its_duration() {
        // Emitting `"duration": 0` for a mutant that never ran would show up in the viewer as a
        // suspiciously fast result rather than as no result.
        let source = SourceFile::parse("src/lib.rs", "fn f() {}".to_owned()).expect("parses");

        assert_eq!(render(&mutant(Outcome::Ignored, 0..1), &source).duration, None);

        let mut timed = mutant(Outcome::Killed, 0..1);

        timed.elapsed_ms = 12;

        assert_eq!(render(&timed, &source).duration, Some(12));
    }

    #[test]
    fn only_settled_mutants_are_carried_forward() {
        // A survivor has to be retried, because the next run's tests may kill it. A killed mutant
        // never will be, so rerunning it is pure cost.
        let text = r#"{
            "schemaVersion": "2",
            "thresholds": { "high": 80, "low": 60 },
            "framework": { "name": "cargo-gamma", "version": "0.1.0" },
            "files": {
                "src/lib.rs": {
                    "language": "rust",
                    "source": "src/lib.rs",
                    "mutants": [
                        { "id": "a", "mutatorName": "m", "status": "Killed", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "b", "mutatorName": "m", "status": "Survived", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "c", "mutatorName": "m", "status": "Timeout", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } },
                        { "id": "d", "mutatorName": "m", "status": "NoCoverage", "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } } }
                    ]
                }
            }
        }"#;

        let settled = settled_mutants(text).expect("parses");
        let mut ids: Vec<&str> = settled.iter().map(String::as_str).collect();

        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn a_file_without_mutants_is_left_out_of_the_report() {
        let plan = Plan {
            root: Utf8PathBuf::from("/w"),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute: Utf8PathBuf::from("/w/src/lib.rs"),
                package: "subject".to_owned(),
            }],
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };

        let report = build(&plan, Thresholds::default(), None).expect("report builds");

        // Embedding every selected file would bloat empty reports and try to read files that have
        // no result to show.
        assert!(report.files.is_empty());
    }

    #[test]
    fn an_unparsable_prior_report_is_reported_rather_than_ignored() {
        let _cause = settled_mutants("not json").unwrap_err();
    }

    #[test]
    fn the_document_serializes_with_the_schema_field_names() {
        let report = Report {
            schema_version: SCHEMA_VERSION.to_owned(),
            thresholds: Thresholds::default(),
            project_root: None,
            framework: Framework {
                name: "cargo-gamma".to_owned(),
                version: "0.1.0".to_owned(),
            },
            files: HashMap::default(),
            config: None,
        };
        let json = to_json(&report).expect("serializes");

        assert!(json.contains("\"schemaVersion\": \"2\""), "{json}");
        assert!(json.contains("\"thresholds\""), "{json}");
        assert!(json.contains("\"files\""), "{json}");
        assert!(!json.contains("projectRoot"), "{json}");
    }

    #[test]
    fn writing_a_report_creates_parent_directories() {
        let path = Utf8PathBuf::from("target/cargo-gamma-elements-write-test/report.json");

        let _ = fs::remove_file(path.as_std_path());
        let _ = fs::remove_dir(path.parent().expect("parent").as_std_path());

        // Report paths are often nested artifact locations; the caller should not have to create
        // the directory tree separately.
        write(&path, "{}").expect("write report");

        let contents = fs::read_to_string(path.as_std_path()).expect("read report");

        assert_eq!(contents, "{}");

        let _ = fs::remove_file(path.as_std_path());
        let _ = fs::remove_dir(path.parent().expect("parent").as_std_path());
    }
}
