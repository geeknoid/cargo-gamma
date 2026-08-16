use camino::Utf8Path;
use compact_str::CompactString;
use core::ops::Range;
use std::sync::Arc;

use crate::ops::collect::Shape;

use super::{MutantId, MutationSite};

/// One source-level mutation, before Cargo/run policy and execution state are attached.
#[derive(Debug, Clone)]
pub struct MutantDefinition {
    pub id: MutantId,
    pub file: Arc<Utf8Path>,
    /// Shared site data (span, location, original text) for all replacements at this span.
    pub site: Arc<MutationSite>,
    pub mutator: Arc<str>,
    pub item_path: Arc<str>,
    pub occurrence: u32,
    pub replacement_index: u32,
    pub replacement: CompactString,
    pub shape: Shape,
}

impl MutantDefinition {
    /// Byte range of the construct in the original file.
    #[inline]
    #[must_use]
    pub fn span(&self) -> &Range<usize> {
        &self.site.span
    }

    /// One-based start line.
    #[inline]
    #[must_use]
    pub fn line(&self) -> usize {
        self.site.line
    }

    /// One-based end line.
    #[inline]
    #[must_use]
    pub fn end_line(&self) -> usize {
        self.site.end_line
    }

    /// One-based start column.
    #[inline]
    #[must_use]
    pub fn column(&self) -> usize {
        self.site.column
    }

    /// The original source text of the construct.
    #[inline]
    #[must_use]
    pub fn original(&self) -> &CompactString {
        &self.site.original
    }
}
