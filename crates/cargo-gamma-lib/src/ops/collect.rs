pub use cargo_gamma_engine::ops::collect::{Candidate, Defaults, Shape, check_stated, collect, collect_in, collect_with};

use std::sync::Arc;

use cargo_gamma_engine::ops::collect::into_definitions;

use crate::model::Mutant;
use crate::parse::SourceFile;

/// Attaches Cargo package and neutral run state to source-level mutant definitions.
#[must_use]
pub fn into_mutants(file: &SourceFile, package: &str, candidates: Vec<Candidate>) -> Vec<Mutant> {
    let package = Arc::from(package);

    into_definitions(file, candidates)
        .into_iter()
        .map(|definition| Mutant::from_definition(definition, Arc::clone(&package)))
        .collect()
}
