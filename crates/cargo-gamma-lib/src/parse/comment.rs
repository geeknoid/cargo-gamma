//! Comment trivia: scanning raw source text for comments that `syn` never sees.

use core::ops::Range;

/// What kind of comment was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// `// ...`, the only kind that can carry a directive.
    Line,

    /// `/// ...` or `/** ... */`, documentation for the item that follows.
    OuterDoc,

    /// `//! ...` or `/*! ... */`, documentation for the enclosing item.
    InnerDoc,

    /// `/* ... */`.
    Block,
}

/// One comment, located in the file.
#[derive(Debug, Clone)]
pub struct Comment {
    /// What kind of comment this is.
    pub kind: CommentKind,

    /// Byte range of the comment including its delimiters.
    pub span: Range<usize>,

    /// The comment text with its delimiters and one leading space removed.
    pub body: String,

    /// 1-based line number of the comment's first line.
    pub line: usize,

    /// Whether anything other than whitespace precedes the comment on its own line.
    pub trailing: bool,
}

/// Scans source text for comments, skipping over anything inside a string or character literal.
pub(super) fn scan_comments(text: &str, lines: &[usize]) -> Vec<Comment> {
    let bytes = text.as_bytes();
    let mut comments = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let start = i;
                let end = line_comment_end(bytes, i);
                let raw = text.get(start..end).unwrap_or("");

                comments.push(build_comment(line_comment_kind(raw), raw, start..end, text, lines));
                i = end;
            }

            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                let end = block_comment_end(bytes, i);
                let raw = text.get(start..end).unwrap_or("");

                comments.push(build_comment(block_comment_kind(raw), raw, start..end, text, lines));
                i = end;
            }

            b'"' => i = string_end(bytes, i),
            b'r' if matches!(bytes.get(i + 1), Some(b'"' | b'#')) => i = raw_string_end(bytes, i),
            b'\'' => i = quote_end(bytes, i),
            _ => i += 1,
        }
    }

    comments
}

/// Classifies a `//` comment. Order matters: `//!` and `///` must be checked before `//`.
fn line_comment_kind(raw: &str) -> CommentKind {
    if raw.starts_with("//!") {
        CommentKind::InnerDoc
    } else if raw.starts_with("///") && !raw.starts_with("////") {
        CommentKind::OuterDoc
    } else {
        CommentKind::Line
    }
}

/// Classifies a `/* */` comment.
fn block_comment_kind(raw: &str) -> CommentKind {
    if raw.starts_with("/*!") {
        CommentKind::InnerDoc
    } else if raw.starts_with("/**") && !raw.starts_with("/**/") && !raw.starts_with("/***") {
        CommentKind::OuterDoc
    } else {
        CommentKind::Block
    }
}

/// Builds a comment record, stripping delimiters and working out whether it trails code.
fn build_comment(
    kind: CommentKind,
    raw: &str,
    span: Range<usize>,
    text: &str,
    lines: &[usize],
) -> Comment {
    let stripped = raw
        .trim_start_matches('/')
        .trim_start_matches('*')
        .trim_start_matches('!')
        .trim_end_matches('/')
        .trim_end_matches('*');

    let line_index = match lines.binary_search(&span.start) {
        Ok(exact) => exact,
        Err(insertion) => insertion.saturating_sub(1),
    };

    let line_start = lines.get(line_index).copied().unwrap_or(0);
    let before = text.get(line_start..span.start).unwrap_or("");

    Comment {
        kind,
        span,
        body: stripped.trim().to_owned(),
        line: line_index + 1,
        trailing: !before.trim().is_empty(),
    }
}

/// Returns the offset just past the end of a `//` comment.
fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;

    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }

    i
}

/// Returns the offset just past the end of a `/* */` comment, which may nest.
fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    let mut depth = 1_usize;

    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;

            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }

    bytes.len()
}

/// Returns the offset just past the end of a `"..."` literal.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }

    bytes.len()
}

/// Returns the offset just past the end of an `r#".."#` literal.
fn raw_string_end(bytes: &[u8], start: usize) -> usize {
    let mut hashes = 0;
    let mut i = start + 1;

    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }

    if bytes.get(i) != Some(&b'"') {
        // Not a raw string after all, just an identifier beginning with `r`.
        return start + 1;
    }

    i += 1;

    while i < bytes.len() {
        if bytes[i] == b'"' {
            let closing = i + 1;
            let found = bytes[closing..].iter().take_while(|b| **b == b'#').count();

            if found >= hashes {
                return closing + hashes;
            }
        }

        i += 1;
    }

    bytes.len()
}

/// Returns the offset just past a `'`, which may open a character literal or a lifetime.
///
/// This is the one genuinely ambiguous case in Rust's lexical grammar for a scanner this small.
/// Getting it wrong is not cosmetic: treating the `'a` in `fn f<'a>()` as an unterminated
/// character literal would swallow the rest of the file and lose every comment in it.
fn quote_end(bytes: &[u8], start: usize) -> usize {
    // `'\n'`, `'\''`, `'\u{1f600}'` and friends. The escaped character is skipped rather than
    // examined, or the quote in `'\''` would be mistaken for the closing one.
    if bytes.get(start + 1) == Some(&b'\\') {
        let mut i = start + 3;

        while i < bytes.len() && bytes[i] != b'\'' {
            i += 1;
        }

        // An unterminated literal at the end of the file has no closing quote to step past.
        return (i + 1).min(bytes.len());
    }

    // A single character followed by a closing quote is a character literal. The character may be
    // several bytes, so step by whole characters rather than by bytes.
    let rest = &bytes[start + 1..];
    let width = utf8_width(rest.first().copied().unwrap_or(0));

    if rest.get(width) == Some(&b'\'') {
        return start + 1 + width + 1;
    }

    // Otherwise it is a lifetime or a label; consume just the quote.
    start + 1
}

/// Returns the number of bytes in a UTF-8 sequence given its leading byte.
const fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::source_file::{SourceFile, line_starts};

    fn parse(text: &str) -> SourceFile {
        SourceFile::parse("test.rs", text.to_owned()).unwrap()
    }

    #[test]
    fn line_comments_are_found_and_stripped() {
        let file = parse("// hello\nfn f() {}\n");

        assert_eq!(file.comments.len(), 1);
        assert_eq!(file.comments[0].kind, CommentKind::Line);
        assert_eq!(file.comments[0].body, "hello");
        assert_eq!(file.comments[0].line, 1);
        assert!(!file.comments[0].trailing);
    }

    #[test]
    fn doc_comments_are_distinguished_from_directive_comments() {
        let file = parse("//! module\n/// item\n// plain\n//// also plain\nfn f() {}\n");
        let kinds: Vec<CommentKind> = file.comments.iter().map(|c| c.kind).collect();

        assert_eq!(
            kinds,
            vec![
                CommentKind::InnerDoc,
                CommentKind::OuterDoc,
                CommentKind::Line,
                CommentKind::Line
            ]
        );
    }

    #[test]
    fn an_escaped_quote_literal_does_not_confuse_the_scanner() {
        // `'\''` used to stop at the escaped quote, leaving the scanner one quote out of step for
        // the rest of the line.
        assert_eq!(quote_end(br"'\''", 0), 4);
        assert_eq!(quote_end(br"'\n'", 0), 4);
        assert_eq!(quote_end(br"'\u{27}'", 0), 8);
        assert_eq!(quote_end(b"'a'", 0), 3);

        // A lifetime is not a literal, so only the quote is consumed.
        assert_eq!(quote_end(b"'static", 0), 1);
    }

    #[test]
    fn an_unterminated_literal_stays_inside_the_text() {
        // The result indexes the text, so running off the end would be a panic waiting to happen.
        for text in [r"'\'", r"'\", "'"] {
            let bytes = text.as_bytes();

            assert!(quote_end(bytes, 0) <= bytes.len(), "{text} ran past the end");
        }
    }

    #[test]
    fn a_comment_after_an_escaped_quote_is_still_found() {
        let file = parse("fn f() { let _q = '\\''; } // after\n");

        assert_eq!(file.comments.len(), 1);
        assert_eq!(file.comments[0].body, "after");
    }

    #[test]
    fn trailing_comments_are_marked() {
        let file = parse("fn f() { let _x = 1; } // after\n");

        assert!(file.comments[0].trailing);
    }

    #[test]
    fn comment_markers_inside_strings_are_not_comments() {
        let file = parse("fn f() -> &'static str { \"// not a comment\" }\n");

        assert!(file.comments.is_empty(), "{:?}", file.comments);
    }

    #[test]
    fn comment_markers_inside_raw_strings_are_not_comments() {
        let file = parse("fn f() -> &'static str { r#\"// nope /* nope */\"# }\n");

        assert!(file.comments.is_empty(), "{:?}", file.comments);
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let file = parse("fn f() -> &'static str { \"a\\\"// no\" }\n// yes\n");

        assert_eq!(file.comments.len(), 1);
        assert_eq!(file.comments[0].body, "yes");
    }

    #[test]
    fn lifetimes_do_not_swallow_the_rest_of_the_file() {
        let file = parse("fn f<'a>(x: &'a str) -> &'a str { x }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
        assert_eq!(file.comments[0].body, "found me");
    }

    #[test]
    fn character_literals_are_skipped() {
        let file = parse("fn f() -> char { '\\'' }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
        assert_eq!(file.comments[0].body, "found me");
    }

    #[test]
    fn multibyte_character_literals_are_skipped() {
        let file = parse("fn f() -> char { 'é' }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn a_quote_in_a_labelled_loop_is_not_a_literal() {
        let file = parse("fn f() { 'outer: loop { break 'outer; } }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn block_comments_nest() {
        let file = parse("/* outer /* inner */ still outer */\nfn f() {}\n");

        assert_eq!(file.comments.len(), 1);
        assert!(file.comments[0].body.contains("still outer"));
    }

    #[test]
    fn block_doc_comments_are_classified() {
        let text = "/*! module */\n/** item */\n/**/ /* plain */\nfn f() {}\n";
        let file = parse(text);
        let kinds: Vec<CommentKind> = file.comments.iter().map(|c| c.kind).collect();

        assert_eq!(
            kinds,
            vec![
                CommentKind::InnerDoc,
                CommentKind::OuterDoc,
                CommentKind::Block,
                CommentKind::Block
            ]
        );
    }

    #[test]
    fn an_unterminated_block_comment_stops_at_end_of_file() {
        // syn rejects this, so scan the trivia directly.
        let text = "fn f() {} /* never closed";
        let comments = scan_comments(text, &line_starts(text));

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].span.end, text.len());
    }

    #[test]
    fn spans_point_at_the_comment_itself() {
        let file = parse("fn f() {}\n// directive\n");
        let span = file.comments[0].span.clone();

        assert_eq!(file.slice(&span), "// directive");
    }

    #[test]
    fn r_prefixed_identifiers_are_not_raw_strings() {
        let file = parse("fn f() { let range = 1; let _ = range; }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn raw_strings_with_many_hashes_are_handled() {
        let file = parse("fn f() -> &'static str { r###\"a\"# b\"### }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn unterminated_strings_and_raw_strings_stop_at_end_of_file() {
        assert_eq!(string_end(b"\"unterminated", 0), b"\"unterminated".len());
        assert_eq!(raw_string_end(br#"r#"unterminated"#, 0), br#"r#"unterminated"#.len());
    }

    #[test]
    fn invalid_raw_string_prefix_consumes_only_the_r() {
        assert_eq!(raw_string_end(b"range", 0), 1);
    }

    #[test]
    fn multibyte_widths_cover_three_and_four_byte_characters() {
        let euro = "€".as_bytes()[0];
        let crab = "🦀".as_bytes()[0];

        assert_eq!(utf8_width(euro), 3);
        assert_eq!(utf8_width(crab), 4);
    }

    #[test]
    fn comments_come_back_in_source_order() {
        let file = parse("// one\nfn f() {}\n// two\nfn g() {}\n// three\n");
        let bodies: Vec<&str> = file.comments.iter().map(|c| c.body.as_str()).collect();

        assert_eq!(bodies, vec!["one", "two", "three"]);
    }
}
