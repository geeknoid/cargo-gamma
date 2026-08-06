//! The twelve pieces of a SARIF 2.1.0 log document, all serialized together as one wire format.

use serde::Serialize;

use crate::HashMap;

/// A SARIF 2.1.0 log.
#[derive(Debug, Serialize)]
pub(super) struct Log {
    pub(super) version: &'static str,
    #[serde(rename = "$schema")]
    pub(super) schema: &'static str,
    pub(super) runs: Vec<Run>,
}

#[derive(Debug, Serialize)]
pub(super) struct Run {
    pub(super) tool: Tool,
    pub(super) results: Vec<Finding>,
}

#[derive(Debug, Serialize)]
pub(super) struct Tool {
    pub(super) driver: Driver,
}

#[derive(Debug, Serialize)]
pub(super) struct Driver {
    pub(super) name: &'static str,
    #[serde(rename = "informationUri")]
    pub(super) information_uri: &'static str,
    pub(super) version: &'static str,
    pub(super) rules: Vec<Rule>,
}

#[derive(Debug, Serialize)]
pub(super) struct Rule {
    pub(super) id: String,
    pub(super) name: String,
    #[serde(rename = "shortDescription")]
    pub(super) short_description: Text,
    #[serde(rename = "fullDescription")]
    pub(super) full_description: Text,
    #[serde(rename = "defaultConfiguration")]
    pub(super) default_configuration: Configuration,
}

#[derive(Debug, Serialize)]
pub(super) struct Configuration {
    pub(super) level: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct Text {
    pub(super) text: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Finding {
    #[serde(rename = "ruleId")]
    pub(super) rule_id: String,
    pub(super) level: &'static str,
    pub(super) message: Text,
    pub(super) locations: Vec<Location>,
    #[serde(rename = "partialFingerprints")]
    pub(super) partial_fingerprints: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub(super) struct Location {
    #[serde(rename = "physicalLocation")]
    pub(super) physical_location: Physical,
}

#[derive(Debug, Serialize)]
pub(super) struct Physical {
    #[serde(rename = "artifactLocation")]
    pub(super) artifact_location: Artifact,
    pub(super) region: Region,
}

#[derive(Debug, Serialize)]
pub(super) struct Artifact {
    pub(super) uri: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Region {
    #[serde(rename = "startLine")]
    pub(super) start_line: usize,
    #[serde(rename = "startColumn")]
    pub(super) start_column: usize,
}
