//! Parsed source text and byte-offset navigation over it.

use camino::{Utf8Path, Utf8PathBuf};
use core::ops::Range;
use std::fs;
use syn::File;

use super::comment::{self, Comment};
use crate::Result;
use crate::error::error;

/// A parsed source file, with everything downstream stages need to work in byte offsets.
#[derive(Debug)]
pub struct SourceFile {
    /// Path as it should appear in reports, relative to the workspace root where possible.
    pub path: Utf8PathBuf,

    /// The exact bytes that were parsed. All spans index into this.
    pub text: String,

    /// The syntax tree.
    pub ast: File,

    /// Byte offset of the start of each line.
    lines: Vec<usize>,

    /// Every comment in the file, in source order.
    pub comments: Vec<Comment>,
}

impl SourceFile {
    /// Parses source text that has already been read.
    ///
    /// The path is used only for diagnostics and reporting; nothing is read from disk here, which
    /// is what lets every test in this crate work on string literals.
    pub fn parse(path: impl Into<Utf8PathBuf>, text: String) -> Result<Self> {
        let path = path.into();

        let ast = syn::parse_file(&text).map_err(|cause| {
            let start = cause.span().start();

            error!("{path}:{}:{}: could not parse: {cause}", start.line, start.column)
        })?;

        let lines = line_starts(&text);
        let comments = comment::scan_comments(&text, &lines);

        Ok(Self {
            path,
            text,
            ast,
            lines,
            comments,
        })
    }

    /// Reads and parses a file from disk.
    pub fn read(path: impl AsRef<Utf8Path>) -> Result<Self> {
        let path = path.as_ref();

        let text = fs::read_to_string(path)
            .map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

        Self::parse(path.to_owned(), text)
    }

    /// Returns the 1-based line and column of a byte offset.
    ///
    /// The column is counted in characters rather than bytes, because it is shown to humans beside
    /// a rendering of the line, and a byte column would point at the wrong place in any line
    /// containing non-ASCII text.
    #[must_use]
    pub fn location(&self, offset: usize) -> (usize, usize) {
        let line_index = match self.lines.binary_search(&offset) {
            Ok(exact) => exact,
            Err(insertion) => insertion.saturating_sub(1),
        };

        let line_start = self.lines.get(line_index).copied().unwrap_or(0);
        let clamped = offset.min(self.text.len());
        let column = self.text.get(line_start..clamped).map_or(0, |s| s.chars().count());

        (line_index + 1, column + 1)
    }

    /// Returns the 1-based line number of a byte offset.
    #[must_use]
    pub fn line_of(&self, offset: usize) -> usize {
        self.location(offset).0
    }

    /// Returns the text of a 1-based line, without its terminator.
    #[must_use]
    pub fn line_text(&self, line: usize) -> &str {
        let Some(start) = self.lines.get(line.wrapping_sub(1)).copied() else {
            return "";
        };

        let end = self.lines.get(line).copied().unwrap_or(self.text.len());

        self.text.get(start..end).unwrap_or("").trim_end_matches(['\n', '\r'])
    }

    /// Returns the number of lines in the file.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Returns the source text covered by a byte range.
    #[must_use]
    pub fn slice(&self, span: &Range<usize>) -> &str {
        self.text.get(span.start..span.end).unwrap_or("")
    }
}

/// Returns the byte offset at which each line starts.
pub(super) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];

    starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));

    // A trailing newline does not open a line that anything can be on.
    if starts.last() == Some(&text.len()) && !text.is_empty() {
        let _ = starts.pop();
    }

    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> SourceFile {
        SourceFile::parse("test.rs", text.to_owned()).unwrap()
    }

    #[test]
    fn a_parse_failure_names_the_file_and_position() {
        let error = SourceFile::parse("bad.rs", "fn f( {".to_owned()).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("bad.rs"), "{message}");
        assert!(message.contains("could not parse"), "{message}");
    }

    #[test]
    fn read_loads_and_parses_a_file_from_disk() {
        let path = Utf8Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/parse/source_file.rs"));
        let file = SourceFile::read(path).unwrap();

        assert_eq!(file.path, path);
        assert!(file.text.contains("pub struct SourceFile"));
    }

    #[test]
    fn read_failures_name_the_missing_file() {
        let error = SourceFile::read(Utf8Path::new("target/does-not-exist/source.rs")).unwrap_err();

        assert!(error.to_string().contains("could not read"));
    }

    #[test]
    fn locations_are_one_based() {
        let file = parse("fn a() {}\nfn b() {}\n");

        assert_eq!(file.location(0), (1, 1));
        assert_eq!(file.location(3), (1, 4));
        assert_eq!(file.location(10), (2, 1));
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        let file = parse("fn f() { let s = \"éé\"; let _ = s; }\n");
        let offset = file.text.find("let _").unwrap();
        let (line, column) = file.location(offset);

        assert_eq!(line, 1);
        assert_eq!(file.text.get(..offset).unwrap().chars().count() + 1, column);
    }

    #[test]
    fn line_text_excludes_the_terminator() {
        let file = parse("fn a() {}\r\nfn b() {}\n");

        assert_eq!(file.line_text(1), "fn a() {}");
        assert_eq!(file.line_text(2), "fn b() {}");
        assert_eq!(file.line_text(99), "");
    }

    #[test]
    fn a_trailing_newline_does_not_open_a_line() {
        assert_eq!(parse("fn a() {}\n").line_count(), 1);
        assert_eq!(parse("fn a() {}\nfn b() {}\n").line_count(), 2);
    }
}
