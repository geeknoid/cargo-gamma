//! Validates that the emitted report conforms to the published `mutation-testing-elements` schema.
//!
//! The schema is an artifact of another project, so drift is silent: a renamed field or a newly
//! required one does not break a build, it produces a blank page in someone's browser weeks later.
//! This gate reads the vendored schema document and checks the emitter against it, so the failure
//! arrives here instead.
//!
//! It deliberately checks the constraints that can actually break a report — required fields, the
//! closed status enum and the version pattern — rather than reimplementing JSON Schema. A full
//! validator would cost two hundred transitive dependencies to catch cases the emitter cannot
//! produce.

use cargo_gamma_lib::elements::{FileResult, Framework, Location, MutantResult, Position, Report, Thresholds, to_json};
use serde_json::Value;
use std::collections::HashMap;

/// The schema document, vendored beside the viewer it describes.
const SCHEMA: &str = include_str!("../src/vendor/mutation-testing-report-schema.json");

/// Parses the vendored schema.
fn schema() -> Value {
    serde_json::from_str(SCHEMA).expect("the vendored schema is not valid JSON")
}

/// Returns the string entries of an array-valued key.
fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .expect("the schema has no array at this key")
        .iter()
        .map(|entry| entry.as_str().expect("a non-string entry").to_owned())
        .collect()
}

/// Navigates to the `MutantResult` subschema.
fn mutant_schema(root: &Value) -> &Value {
    &root["properties"]["files"]["additionalProperties"]["properties"]["mutants"]["items"]
}

/// Builds a report exercising every field the emitter can produce.
fn sample() -> Report {
    let mutant = MutantResult {
        id: "abc123abc123".to_owned(),
        mutator_name: "relational.lt_to_le".to_owned(),
        location: Location {
            start: Position { line: 2, column: 5 },
            end: Position { line: 2, column: 10 },
        },
        status: "Survived".to_owned(),
        replacement: Some("(a) <= (b)".to_owned()),
        description: Some("replace a < b with (a) <= (b)".to_owned()),
        status_reason: Some("suppressed by comment".to_owned()),
        duration: Some(12),
        killed_by: Some(vec!["tests::boundary".to_owned()]),
    };

    let mut files = HashMap::new();

    let _ = files.insert(
        "src/lib.rs".to_owned(),
        FileResult {
            source: "fn f() {}\n".to_owned(),
            language: "rust".to_owned(),
            mutants: vec![mutant],
        },
    );

    Report {
        schema_version: "2".to_owned(),
        thresholds: Thresholds::default(),
        project_root: Some("/work".to_owned()),
        framework: Framework {
            name: "cargo-gamma".to_owned(),
            version: "0.1.0".to_owned(),
        },
        files: files.into_iter().collect(),
        config: None,
    }
}

/// Serializes the sample report as a JSON value.
fn emitted() -> Value {
    serde_json::from_str(&to_json(&sample()).expect("serializes")).expect("emits valid JSON")
}

#[test]
fn the_document_has_every_top_level_required_field() {
    let root = schema();
    let document = emitted();

    for field in strings(&root, "required") {
        assert!(document.get(&field).is_some(), "the report is missing `{field}`");
    }
}

#[test]
fn every_file_entry_has_the_required_fields() {
    let root = schema();
    let document = emitted();
    let required = strings(&root["properties"]["files"]["additionalProperties"], "required");

    for (path, file) in document["files"].as_object().expect("files is an object") {
        for field in &required {
            assert!(file.get(field).is_some(), "`{path}` is missing `{field}`");
        }
    }
}

#[test]
fn every_mutant_has_the_required_fields() {
    let root = schema();
    let document = emitted();
    let required = strings(mutant_schema(&root), "required");

    for file in document["files"].as_object().expect("files is an object").values() {
        for mutant in file["mutants"].as_array().expect("mutants is an array") {
            for field in &required {
                assert!(mutant.get(field).is_some(), "a mutant is missing `{field}`: {mutant}");
            }
        }
    }
}

#[test]
fn every_status_we_emit_is_in_the_schemas_closed_enum() {
    // `MutantStatus` has no room for invention. A value outside this list is rejected by the
    // viewer, which renders as an empty report rather than as an error anyone can read.
    let root = schema();
    let allowed = strings(&mutant_schema(&root)["properties"]["status"], "enum");

    for outcome in ["Pending", "Killed", "Survived", "Timeout", "CompileError", "Ignored", "NoCoverage"] {
        assert!(
            allowed.contains(&outcome.to_owned()),
            "`{outcome}` is not in the schema's status enum: {allowed:?}"
        );
    }
}

#[test]
fn the_emitted_schema_version_matches_the_schemas_own_pattern() {
    // The npm package is at 3.x while the schema accepts major 1 and 2 only. Emitting "3" fails
    // validation for a reason that looks exactly like a version-skew bug and is not.
    let root = schema();
    let pattern = root["properties"]["schemaVersion"]["pattern"]
        .as_str()
        .expect("the schema has no version pattern");

    assert_eq!(pattern, r"^([1-2])(\.(([1-9]\d*)|0)){0,2}$");

    let emitted = emitted();
    let version = emitted["schemaVersion"].as_str().expect("a version is emitted");

    assert!(
        matches!(version, "1" | "2") || version.starts_with("1.") || version.starts_with("2."),
        "`{version}` does not satisfy `{pattern}`"
    );
}

#[test]
fn the_optional_fields_we_emit_are_ones_the_schema_knows() {
    // Extension fields are allowed — no object in the schema sets `additionalProperties: false` —
    // but a *misspelled* known field would be silently accepted and silently ignored, which is the
    // failure this catches.
    let root = schema();
    let known = mutant_schema(&root)["properties"]
        .as_object()
        .expect("the mutant schema has properties");
    let document = emitted();

    for file in document["files"].as_object().expect("files is an object").values() {
        for mutant in file["mutants"].as_array().expect("mutants is an array") {
            for field in mutant.as_object().expect("a mutant is an object").keys() {
                assert!(known.contains_key(field), "`{field}` is not a schema field");
            }
        }
    }
}
