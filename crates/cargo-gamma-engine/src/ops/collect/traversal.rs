//! Driving the collector over a syntax tree to produce the candidates a file admits.

use syn::visit::Visit;

use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

use super::collector::Collector;
use super::{Candidate, Defaults};

/// Collects every candidate a file admits under the given selection, with nothing stripped for
/// configuration.
///
/// Equivalent to [`collect_in`] with an unconditional set. Use it where the build's configuration
/// is not known, which is every caller that is examining a fragment of source rather than a real
/// workspace.
///
/// The result is sorted by span start, then by mutator name, so that two runs over the same source
/// produce the same order regardless of how the traversal happened to visit siblings.
#[must_use]
pub fn collect(file: &SourceFile, selection: &Selection) -> Vec<Candidate> {
    collect_in(file, selection, &CfgSet::unconditional())
}

/// Collects every candidate a file admits, under a selection and a build configuration.
///
/// `cfg` decides which conditionally compiled code is actually in the build. Code behind a
/// predicate that does not hold produces no candidates at all: the compiler strips it, so a guard
/// there would never be compiled and its mutant could never be activated by any test.
#[must_use]
pub fn collect_in(file: &SourceFile, selection: &Selection, cfg: &CfgSet) -> Vec<Candidate> {
    collect_with(file, selection, cfg, &Defaults::default())
}

/// Collects every candidate a file admits, told what the rest of the workspace implements.
///
/// The extra argument is what lets a `Default::default()` be withheld for a type the workspace
/// defines and gives no `Default`. An empty index is not a claim that nothing has one; it says
/// nothing was looked at, and every type stays optimistic, which is what [`collect_in`] passes.
#[must_use]
pub fn collect_with(file: &SourceFile, selection: &Selection, cfg: &CfgSet, defaults: &Defaults) -> Vec<Candidate> {
    let mut collector = Collector::new(file, selection, selection.errors(), cfg, defaults);

    collector.visit_file(&file.ast);

    let mut candidates = collector.finish();

    candidates.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| left.span.end.cmp(&right.span.end))
            .then_with(|| left.mutator.cmp(right.mutator))
            .then_with(|| left.replacement_index.cmp(&right.replacement_index))
    });

    candidates
}
