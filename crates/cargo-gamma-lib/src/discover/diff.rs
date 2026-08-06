//! Restricting a run to the lines a unified diff touches.

use camino::{Utf8Path, Utf8PathBuf};
use std::fs;
use std::io::{Read, stdin};

use crate::HashMap;
use crate::Result;
use crate::error::error;

/// The lines each file gained in a diff, as a set of line numbers in the new file.
#[derive(Debug, Default)]
pub struct Diff {
    touched: HashMap<Utf8PathBuf, Vec<u32>>,
}

impl Diff {
    /// Reads a unified diff from a path, or from standard input when the path is `-`.
    ///
    /// # Errors
    ///
    /// Returns an error if the diff cannot be read.
    pub fn read(path: &Utf8Path) -> Result<Self> {
        Self::read_from(path, &mut stdin())
    }

    /// Reads a unified diff, taking `-` from `input` rather than from the real standard input.
    ///
    /// The seam exists so that the `-` path is an ordinary test rather than something that would
    /// block on a terminal, which is what reading the process's real standard input would do inside
    /// a test binary.
    ///
    /// # Errors
    ///
    /// Returns an error if the diff cannot be read.
    pub fn read_from(path: &Utf8Path, input: &mut impl Read) -> Result<Self> {
        let text = if path == "-" {
            let mut buffer = String::new();

            let _read = input
                .read_to_string(&mut buffer)
                .map_err(|cause| error!("could not read a diff from standard input").caused_by(cause))?;

            buffer
        } else {
            fs::read_to_string(path).map_err(|cause| error!("could not read the diff `{path}`").caused_by(cause))?
        };

        Ok(Self::parse(&text))
    }

    /// Parses a unified diff.
    ///
    /// Only added and modified lines count. A deleted line has no position in the new file, so
    /// there is nothing there to mutate, and a context line is by definition unchanged.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut touched: HashMap<Utf8PathBuf, Vec<u32>> = HashMap::default();
        let mut current: Option<Utf8PathBuf> = None;
        let mut line_number = 0_u32;

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                current = new_file_path(rest);
                continue;
            }

            if line.starts_with("--- ") {
                continue;
            }

            if let Some(rest) = line.strip_prefix("@@") {
                if let Some(start) = hunk_start(rest) {
                    line_number = start;
                } else {
                    current = None;
                }

                continue;
            }

            let Some(path) = current.as_ref() else {
                continue;
            };

            match line.as_bytes().first() {
                Some(b'+') => {
                    touched.entry(path.clone()).or_default().push(line_number);
                    line_number = line_number.saturating_add(1);
                }

                // A context or deleted line: only the former advances the new file's numbering.
                Some(b'-') => {}
                Some(b' ') | None => line_number = line_number.saturating_add(1),

                // Anything else ends the hunk: `\ No newline at end of file`, a `diff --git`
                // header, or prose wrapped around the patch.
                Some(_other) => {}
            }
        }

        Self { touched }
    }

    /// Returns whether the diff mentions any file at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.touched.is_empty()
    }

    /// Returns whether a file has any changed line.
    #[must_use]
    pub fn touches_file(&self, path: &Utf8Path) -> bool {
        self.touched.contains_key(path)
    }

    /// Returns whether a region of a file overlaps anything the diff changed.
    ///
    /// A mutation site is matched by its whole extent rather than by its first line, so editing
    /// the middle of a multi-line condition still selects the mutants on it.
    #[must_use]
    pub fn touches(&self, path: &Utf8Path, start: u32, end: u32) -> bool {
        self.touched
            .get(path)
            .is_some_and(|lines| lines.iter().any(|line| *line >= start && *line <= end))
    }
}

/// Extracts the path from a `+++` header, rejecting the one that means "the file was deleted".
fn new_file_path(rest: &str) -> Option<Utf8PathBuf> {
    // Trailing tab-separated metadata is part of the format, and git writes a timestamp there.
    let path = rest.split('\t').next().unwrap_or(rest).trim();

    if path == "/dev/null" {
        return None;
    }

    // `b/` is git's prefix for the post-image. A plain `diff -u` has no prefix, so it is stripped
    // only when present rather than assumed.
    let path = path.strip_prefix("b/").unwrap_or(path);

    if path.is_empty() { None } else { Some(Utf8PathBuf::from(path)) }
}

/// Reads the first line number of a hunk's post-image from an `@@ -a,b +c,d @@` header.
fn hunk_start(rest: &str) -> Option<u32> {
    let plus = rest.split('+').nth(1)?;
    let digits: String = plus.chars().take_while(char::is_ascii_digit).collect();

    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1234567..89abcde 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,6 +10,8 @@ fn existing() {
 context one
 context two
+added at twelve
+added at thirteen
 context three
-removed line
 context four
";

    #[test]
    fn added_lines_are_numbered_in_the_new_file() {
        let diff = Diff::parse(SAMPLE);
        let path = Utf8Path::new("src/lib.rs");

        assert!(diff.touches(path, 12, 12));
        assert!(diff.touches(path, 13, 13));
        assert!(!diff.touches(path, 11, 11));
        assert!(!diff.touches(path, 14, 20));
    }

    #[test]
    fn the_git_prefix_is_stripped() {
        assert!(Diff::parse(SAMPLE).touches_file(Utf8Path::new("src/lib.rs")));
    }

    #[test]
    fn a_deleted_file_contributes_nothing() {
        let diff = Diff::parse("--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n");

        assert!(diff.is_empty());
    }

    #[test]
    fn a_region_spanning_a_changed_line_is_selected() {
        let diff = Diff::parse(SAMPLE);

        // A mutation site running from line 10 to line 14 encloses the added lines.
        assert!(diff.touches(Utf8Path::new("src/lib.rs"), 10, 14));
    }

    #[test]
    fn a_diff_without_git_prefixes_is_understood() {
        let diff = Diff::parse("--- old.rs\t2020-01-01\n+++ new.rs\t2020-01-02\n@@ -1 +1,2 @@\n one\n+two\n");

        assert!(diff.touches(Utf8Path::new("new.rs"), 2, 2));
    }

    #[test]
    fn several_hunks_in_one_file_all_count() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1,1 +1,2 @@\n one\n+two\n@@ -50,1 +51,2 @@\n fifty\n+fifty two\n";
        let diff = Diff::parse(text);
        let path = Utf8Path::new("x.rs");

        assert!(diff.touches(path, 2, 2));
        assert!(diff.touches(path, 52, 52));
        assert!(!diff.touches(path, 30, 40));
    }

    #[test]
    fn several_files_are_kept_apart() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1,2 @@\n one\n+x change\n--- a/y.rs\n+++ b/y.rs\n@@ -1 +1,2 @@\n one\n+y change\n";
        let diff = Diff::parse(text);

        assert!(diff.touches(Utf8Path::new("x.rs"), 2, 2));
        assert!(diff.touches(Utf8Path::new("y.rs"), 2, 2));
        assert!(!diff.touches_file(Utf8Path::new("z.rs")));
    }

    #[test]
    fn an_empty_diff_touches_nothing() {
        assert!(Diff::parse("").is_empty());
    }

    // `--in-diff <PATH>` is how a pull-request job scopes a run, so reading the diff off disk has
    // to produce the same answer as parsing the same bytes in memory.
    #[test]
    fn a_diff_is_read_from_a_file() {
        let dir = tempfile::tempdir().expect("could not create a temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("change.patch")).expect("the temporary path is not UTF-8");

        fs::write(&path, SAMPLE).expect("could not write the diff");

        let diff = Diff::read(&path).expect("could not read the diff");

        assert!(!diff.is_empty());
        assert!(diff.touches_file(Utf8Path::new("src/lib.rs")));
    }

    // A diff that does not exist is a usage mistake a caller has to be told about, not an empty
    // selection that would silently mutate nothing and report a perfect score.
    #[test]
    fn a_missing_diff_file_is_an_error_naming_the_path() {
        let error = Diff::read(Utf8Path::new("no/such/change.patch")).expect_err("a missing diff must not parse");

        assert!(error.to_string().contains("no/such/change.patch"), "{error}");
    }

    // `--in-diff -` is the form `git diff | cargo gamma run --in-diff -` uses, and it has to read
    // the whole stream rather than the first line of it.
    #[test]
    fn a_diff_is_read_from_standard_input() {
        let mut input = SAMPLE.as_bytes();
        let diff = Diff::read_from(Utf8Path::new("-"), &mut input).expect("could not read the diff");

        assert!(diff.touches_file(Utf8Path::new("src/lib.rs")));
    }

    // A stream that fails half way through must be reported rather than silently truncated into a
    // diff that touches less than the real change did.
    #[test]
    fn a_failing_standard_input_is_an_error() {
        struct Broken;

        impl Read for Broken {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("the pipe broke"))
            }
        }

        let error = Diff::read_from(Utf8Path::new("-"), &mut Broken).expect_err("a broken pipe must not parse");

        assert!(error.to_string().contains("standard input"), "{error}");
    }

    // A hunk header the parser cannot read leaves it with no idea which line the following `+`
    // lines are at, so the file is abandoned rather than credited with invented line numbers.
    #[test]
    fn an_unreadable_hunk_header_abandons_the_file() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ nonsense @@\n+added\n";
        let diff = Diff::parse(text);

        assert!(diff.is_empty(), "an unparsable hunk must not contribute lines");
    }

    // Real patches carry lines that belong to neither the pre- nor the post-image. They must not
    // advance the line counter, or every mutant after them would be attributed to the wrong line.
    #[test]
    fn a_line_that_is_not_part_of_the_hunk_does_not_advance_the_count() {
        let text = "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n one\n-old\n\\ No newline at end of file\n+new\n";
        let diff = Diff::parse(text);

        // `one` is line 1, the deletion has no post-image line, the `\` marker is not a line
        // either, so the addition is line 2 rather than line 3.
        assert!(diff.touches(Utf8Path::new("x.rs"), 2, 2), "the addition landed on the wrong line");
    }
}
