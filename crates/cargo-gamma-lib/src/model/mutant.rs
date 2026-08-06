use camino::Utf8PathBuf;
use core::ops::Range;
use serde::{Deserialize, Serialize};

use crate::ops::collect::Shape;

use super::outcome::Outcome;
use super::suppression::Suppression;

/// What a directive says a mutant's fate should be, and where the claim was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expectation {
    /// Whether the mutant is expected to survive or to be caught.
    pub caught: bool,

    /// One-based line of the directive that made the claim.
    pub line: usize,

    /// The stated reason, if any.
    pub reason: Option<String>,
}

/// One mutant: a single change to a single site, with a stable identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mutant {
    /// Content-addressed stable identity, 12 lowercase hex characters.
    pub id: String,

    /// One-based ordinal within this run, used as the value of `GAMMA_ACTIVE`.
    ///
    /// Unlike [`Mutant::id`] this is *not* stable across runs; it is a compact selector.
    pub ordinal: u32,

    /// Path relative to the workspace root, with forward slashes.
    pub file: Utf8PathBuf,

    /// The package the file belongs to.
    pub package: String,

    /// Byte range of the mutated construct in the original file.
    pub span: Range<usize>,

    /// One-based line of the start of the span.
    pub line: usize,

    /// One-based column of the start of the span.
    pub column: usize,

    /// The registry name of the mutator that produced this mutant.
    pub mutator: String,

    /// Path of the enclosing item, such as `parser::Lexer::next_token`.
    pub item_path: String,

    /// Index among identical normalized sites within the enclosing item.
    pub occurrence: u32,

    /// Index among the replacements this mutator offers at this site.
    pub replacement_index: u32,

    /// The original source text of the construct, for display.
    pub original: String,

    /// The replacement source text, for display and for splicing.
    pub replacement: String,

    /// How the site must be guarded when the schema is instrumented.
    pub shape: Shape,

    /// The verdict.
    pub outcome: Outcome,

    /// Why it was suppressed, when it was.
    pub suppression: Option<Suppression>,

    /// What an `expect_missed` or `expect_caught` directive says this mutant's fate should be.
    ///
    /// Recorded at discovery, checked once the run has a verdict for it. An expectation is a claim
    /// about the test suite that the author wants held to — "this site is deliberately untested" or
    /// "this site must stay covered" — so it is worth failing the run when reality diverges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expectation: Option<Expectation>,

    /// Wall time spent deciding this mutant, in milliseconds.
    pub elapsed_ms: u64,

    /// The name of the test that killed it, when one did.
    pub killed_by: Option<String>,

    /// Anything else worth saying about the verdict.
    ///
    /// Separate from `killed_by` because that field means one specific thing — the test whose
    /// failure detected the mutant, and the report publishes it under that name.
    pub note: Option<String>,
}

impl Mutant {
    /// Renders a one-line human description, in the form used by `list` and the console reporter.
    ///
    /// A mutated construct can span many lines. Emitting it verbatim would break the one-line
    /// contract that makes this output greppable, so it is flattened and elided in the middle: the
    /// two ends are what identify the construct, and the middle is what is least informative.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{}:{}:{}: {} [{}]",
            self.file,
            self.line,
            self.column,
            self.summary(),
            self.mutator
        )
    }

    /// Renders just the change, with no location.
    ///
    /// Reports that carry the location in a field of their own would otherwise repeat it in the
    /// prose beside it.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.shape {
            Shape::Stmt => format!("delete {}", one_line(&self.original, 56)),
            // The replacement is a guard the reader never sees, so describing it would say
            // nothing. What changed is that the arm stops matching.
            Shape::Arm => format!("stop the arm matching {} from matching", one_line(&self.original, 48)),
            Shape::Expr | Shape::Block => format!(
                "replace {} with {}",
                one_line(&self.original, 40),
                one_line(&self.replacement, 32)
            ),
        }
    }
}

/// Collapses text to a single line of at most `width` characters.
#[must_use]
pub fn one_line(text: &str, width: usize) -> String {
    let flattened: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    let length = flattened.chars().count();

    if length <= width {
        return flattened;
    }

    let head = width.saturating_sub(3) / 2;
    let tail = width.saturating_sub(3) - head;

    flattened
        .chars()
        .take(head)
        .chain("...".chars())
        .chain(flattened.chars().skip(length - tail))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_is_always_one_line() {
        let text = "a\n  +\n  b";

        assert_eq!(one_line(text, 48), "a + b");
    }

    #[test]
    fn a_long_construct_is_elided_in_the_middle() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india";
        let short = one_line(text, 20);

        assert_eq!(short.chars().count(), 20);
        assert!(short.contains("..."), "{short}");
        assert!(short.starts_with("alpha"), "{short}");
        assert!(short.ends_with("india"), "{short}");
    }

    #[test]
    fn a_short_construct_is_left_alone() {
        assert_eq!(one_line("a + b", 48), "a + b");
    }

    #[test]
    fn eliding_counts_characters_not_bytes() {
        let text = "ééééééééééééééééééééééééé";

        assert_eq!(one_line(text, 10).chars().count(), 10);
    }
}
