//! Turning the candidates a file admits into mutant definitions with stable identities.

use camino::Utf8Path;
use compact_str::CompactString;
use std::sync::Arc;

use crate::HashMap;
use crate::model::{Interner, MutantDefinition, MutationSite, mutant_id_with_discriminator, normalize_site_text, site_key};
use crate::parse::SourceFile;

use super::Candidate;

/// Turns candidates into source-level mutant definitions, assigning stable ids.
#[must_use]
pub fn into_definitions(file: &SourceFile, candidates: Vec<Candidate>) -> Vec<MutantDefinition> {
    let mut occurrences: HashMap<u128, u32> = HashMap::default();

    // One copy of the path for the whole file, rather than one per mutation.
    let path: Arc<Utf8Path> = Arc::from(Utf8Path::new(file.path.as_str()));

    // The mutator names and item paths repeat within a file too — a few dozen distinct values
    // across every mutant it produces — so they are shared as they are met. Sharing them across
    // files as well is the survey's job, once it has the whole population.
    let mut interner = Interner::default();

    // Per-span site table: candidates that target the same byte range share one MutationSite.
    let mut sites: HashMap<core::ops::Range<usize>, Arc<MutationSite>> = HashMap::default();

    let mut definitions = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let site = Arc::clone(sites.entry(candidate.span.clone()).or_insert_with(|| {
            let original = CompactString::new(file.slice(&candidate.span));
            let (line, column) = file.location(candidate.span.start);
            let end_line = file.location(candidate.span.end).0;
            Arc::new(MutationSite {
                span: candidate.span.clone(),
                line,
                end_line,
                column,
                original,
            })
        }));

        let normalized = normalize_site_text(&site.original);
        let occurrence = occurrences
            .entry(site_key(&candidate.item_path, candidate.mutator, &normalized))
            .or_insert(0);

        let index = *occurrence;

        *occurrence += 1;

        definitions.push(MutantDefinition {
            id: mutant_id_with_discriminator(
                &file.path,
                &candidate.item_path,
                candidate.mutator,
                &normalized,
                index,
                candidate.replacement_index,
                (candidate.mutator == "fn_value.err_with").then_some(candidate.replacement.as_str()),
            ),
            file: Arc::clone(&path),
            site,
            mutator: interner.text(candidate.mutator),
            item_path: interner.text(&candidate.item_path),
            occurrence: index,
            replacement_index: candidate.replacement_index,
            replacement: candidate.replacement,
            shape: candidate.shape,
        });
    }

    definitions
}
