//! Applying color, or not, according to one explicitly carried decision.

use owo_colors::{OwoColorize as _, Style};

use crate::model::Outcome;

use super::VERB_WIDTH;

/// Applies styling, or does not, according to one explicitly carried decision.
///
/// `owo_colors` offers a process-global override for this, and using it would be a mistake. A
/// global is shared state: two runs in one process — which is exactly what the integration tests
/// are — would race, and whichever ran second would silently decide the first one's colors. The
/// decision belongs to the run that made it, so it is passed down instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Styler {
    enabled: bool,
}

impl Styler {
    /// Creates a styler from an already-resolved decision.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Applies a style to text.
    #[must_use]
    pub fn apply(self, text: &str, style: Style) -> String {
        if self.enabled {
            text.style(style).to_string()
        } else {
            text.to_owned()
        }
    }

    /// Applies a style to text right-aligned in the status column.
    ///
    /// The alignment has to happen here rather than at the format site. A format width counts the
    /// bytes of what it is padding, and a styled string is mostly escape sequences, so
    /// `{styled:>VERB_WIDTH$}` measures the escapes and pads to nothing — which silently collapsed
    /// the whole column whenever color was on.
    #[must_use]
    fn column(self, text: &str, style: Style) -> String {
        self.apply(&format!("{text:>VERB_WIDTH$}"), style)
    }

    /// Renders a cargo-shaped status verb, aligned in the status column.
    #[must_use]
    pub fn verb(self, text: &str) -> String {
        self.column(text, Style::new().bold().green())
    }

    /// Renders a status verb for something that was skipped rather than done.
    #[must_use]
    pub fn note(self, text: &str) -> String {
        self.column(text, Style::new().bold().cyan())
    }

    /// Renders an error label. Not column-aligned, since it introduces a sentence rather than
    /// heading a status line.
    #[must_use]
    pub fn error(self, text: &str) -> String {
        self.apply(text, Style::new().bold().red())
    }

    /// Renders the label for an outcome, aligned in the status column.
    #[must_use]
    pub fn outcome(self, outcome: Outcome) -> String {
        let (text, style) = outcome_style(outcome);

        self.column(text, style)
    }
}

/// The word and color that stand for an outcome.
const fn outcome_style(outcome: Outcome) -> (&'static str, Style) {
    match outcome {
        Outcome::Killed => ("caught", Style::new().green()),
        Outcome::Survived => ("MISSED", Style::new().red().bold()),
        Outcome::Timeout => ("TIMEOUT", Style::new().yellow()),
        // Bold, like `MISSED`, because it is the other outcome that usually means something is
        // wrong with the run rather than with the code: a ceiling too tight convicts healthy
        // mutants, and that has to be noticeable rather than blend into the caught ones.
        Outcome::OutOfMemory => ("OUTOFMEM", Style::new().magenta().bold()),
        Outcome::NoCoverage => ("uncovered", Style::new().yellow()),
        Outcome::CompileError => ("unviable", Style::new()),
        Outcome::Ignored => ("skipped", Style::new()),
        Outcome::NotBuilt => ("notbuilt", Style::new()),
        Outcome::Pending => ("pending", Style::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_are_distinct() {
        let outcomes = [
            Outcome::Killed,
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::CompileError,
            Outcome::NoCoverage,
            Outcome::Ignored,
            Outcome::Pending,
        ];

        let styler = Styler::new(false);
        let labels: crate::HashSet<String> = outcomes.into_iter().map(|o| styler.outcome(o)).collect();

        assert_eq!(labels.len(), outcomes.len());
    }

    #[test]
    fn a_disabled_styler_emits_no_escape_sequences() {
        let styler = Styler::new(false);

        assert_eq!(styler.verb("Found").trim(), "Found");
        assert_eq!(styler.error("error:"), "error:");
        assert!(!styler.outcome(Outcome::Survived).contains('\x1b'));
    }

    #[test]
    fn an_enabled_styler_emits_escape_sequences() {
        let styler = Styler::new(true);

        assert!(styler.verb("Found").contains('\x1b'));
        assert!(styler.outcome(Outcome::Survived).contains('\x1b'));
    }

    #[test]
    fn a_label_occupies_the_status_column_even_when_it_is_colored() {
        // The bug this guards against: `{styled:>WIDTH$}` measures escape bytes, so a colored label
        // pads to nothing and the column collapses on every real terminal.
        let plain = Styler::new(false).verb("Found");
        let colorful = Styler::new(true).verb("Found");

        assert_eq!(plain.len(), VERB_WIDTH);
        assert!(colorful.contains(&plain), "the padding must be inside the styling");
    }

    #[test]
    fn every_status_label_fits_the_column() {
        // A label wider than the column pushes its line out of alignment with every other line.
        let styler = Styler::new(false);

        for label in ["Found", "Skipped", "Summary", "Scanning", "Copying", "Rewriting", "Building", "Baseline",
                      "Estimate", "Testing", "Timing", "Hangs", "Rollback", "Also", "Wrote", "Note", "Kept",
                      "Iterating", "Advice", "Merged", "Freshness", "Rotation", "Warning", "Finished", "Suppressed",
                      "Migrated", "Disk", "Scope", "Withdrawn"] {
            assert_eq!(styler.verb(label).len(), VERB_WIDTH, "`{label}` does not fit the status column");
        }
    }

    #[test]
    fn stylers_do_not_share_state() {
        // Two runs in one process must be able to disagree about color. A process-global override
        // would make whichever ran second decide for both.
        let plain = Styler::new(false);
        let colorful = Styler::new(true);

        assert_eq!(plain.verb("Found").trim(), "Found");
        assert!(colorful.verb("Found").contains('\x1b'));
        assert_eq!(plain.verb("Found").trim(), "Found");
    }
}
