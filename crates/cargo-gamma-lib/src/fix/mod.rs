//! Writing suppressions back into the source, the way `cargo clippy --fix` does.
//!
//! A run knows exactly which mutants caused trouble and exactly where they live, so it can write the
//! directive rather than describing it. What makes that safe is a single rule and a single check.
//!
//! **The rule: a surviving mutant is never eligible.** Not by default, not behind a flag, not with a
//! force switch. A survivor is a real gap in the test suite, and a tool that offers to delete gaps
//! from its own denominator is a tool for manufacturing a mutation score. The moment this can hide a
//! survivor, every number the tool reports becomes unfalsifiable — so the refusal is structural: a
//! surviving verdict has no spelling that reaches this module.
//!
//! **The check: verify, do not assert.** A directive placed one line off, or attached to a
//! multi-line expression, can silently suppress a dozen unrelated mutants — including survivors,
//! which is the rule above being violated by accident rather than by design. So after writing,
//! discovery runs again and the suppressed set is compared: every intended mutant must now be
//! suppressed and nothing else may have become suppressed. If either half fails, the whole edit is
//! reverted.

mod edit;
mod eligible;
mod verification;

use core::cmp::Reverse;
use core::fmt::Write as _;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};

use crate::model::{Mutant, Outcome};

pub use edit::Edit;
pub use eligible::Eligible;
pub use verification::Verification;

/// Chooses the directives to write for a completed run.
///
/// Mutants at the same site coalesce into one directive naming each mutator, rather than one comment
/// each: five stacked comments above a line is not something anyone will keep.
#[must_use]
pub fn plan(mutants: &[Mutant], eligible: &[Eligible]) -> Vec<Edit> {
    let mut grouped: BTreeMap<(Utf8PathBuf, usize), Edit> = BTreeMap::new();

    for mutant in mutants {
        // Belt and braces. `Eligible` cannot name a survivor, so this can only fire if someone adds
        // a variant later — which is exactly when a second check is worth having.
        if matches!(mutant.outcome, Outcome::Survived) {
            continue;
        }

        let Some(tag) = eligible
            .iter()
            .find(|entry| entry.outcome() == mutant.outcome)
            .map(|entry| entry.tag())
        else {
            continue;
        };

        let entry = grouped
            .entry((mutant.file.clone(), mutant.line))
            .or_insert_with(|| Edit {
                file: mutant.file.clone(),
                line: mutant.line,
                mutators: BTreeSet::new(),
                tag,
            });

        let _ = entry.mutators.insert(mutant.mutator.clone());
    }

    grouped.into_values().collect()
}

/// Applies edits to one file's text.
///
/// Edits are applied from the last line backwards so that every earlier line number stays valid; the
/// same discipline the instrumenter needs in the other direction, and the same bug if it is wrong.
///
/// A line that already carries a generated directive has its selector list extended instead of
/// gaining a second comment, which is what makes running this twice a no-op.
#[must_use]
pub fn apply(text: &str, edits: &[&Edit], date: &str) -> String {
    let ending = ending(text);
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let mut ordered: Vec<&&Edit> = edits.iter().collect();

    ordered.sort_by_key(|edit| Reverse(edit.line));

    for edit in ordered {
        let Some(index) = edit.line.checked_sub(1).filter(|index| *index < lines.len()) else {
            continue;
        };

        let indent: String = lines[index]
            .chars()
            .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
            .collect();

        if let Some(above) = index.checked_sub(1)
            && let Some(existing) = generated_selectors(&lines[above])
        {
                let mut merged = edit.mutators.clone();

                merged.extend(existing);

                let rendered = Edit {
                    mutators: merged,
                    ..(*edit).clone()
                }
                .render(&indent, date, ending);

                lines[above] = rendered;
            continue;
        }

        lines.insert(index, edit.render(&indent, date, ending));
    }

    lines.concat()
}

/// The line terminator the file already uses.
///
/// Decided by majority rather than by the first line seen, because a file with one stray ending is
/// still a file with a convention, and matching the stray one would spread it.
fn ending(text: &str) -> &'static str {
    let total = text.matches('\n').count();
    let carriage = text.matches("\r\n").count();

    if carriage * 2 > total { "\r\n" } else { "\n" }
}

/// Returns the selectors of a directive this tool generated, if the line holds one.
///
/// Only *generated* directives are extended. A hand-written directive is someone's decision, with
/// their reason attached, and rewriting it would destroy that reason to save one line.
fn generated_selectors(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim_start();

    if !trimmed.starts_with("// #[gamma::skip(") || !trimmed.contains("written by cargo gamma suppress") {
        return None;
    }

    let inner = trimmed.strip_prefix("// #[gamma::skip(")?;

    Some(
        inner
            .split(',')
            .map(str::trim)
            .take_while(|part| !part.contains('=') && !part.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Returns today's UTC date as `YYYY-MM-DD`.
///
/// Hand-rolled rather than pulling in a date library, because this is the only date the tool ever
/// formats and the conversion is a well-known closed form. The date is what makes a generated
/// directive auditable a year later: "why is this here" is answerable from the comment alone.
#[must_use]
pub fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());

    civil_from_days(i64::try_from(seconds / 86_400).unwrap_or(0))
}

/// Converts a count of days since the Unix epoch into `YYYY-MM-DD`.
///
/// Hinnant's algorithm, which shifts the year to start in March so that the leap day lands at the
/// end and the month-length pattern becomes a single linear expression.
fn civil_from_days(days: i64) -> String {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };
    let year = era * 400 + year_of_era + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}")
}

/// Compares the suppressed sets before and after an edit.
///
/// `intended` is the set of mutant IDs the edit was written for. Both directions matter: an edit
/// that suppresses nothing is a silent no-op, and an edit that suppresses too much is the hazard.
#[must_use]
pub fn verify(before: &[Mutant], after: &[Mutant], intended: &BTreeSet<String>) -> Verification {
    let suppressed = |mutants: &[Mutant]| -> BTreeSet<String> {
        mutants
            .iter()
            .filter(|mutant| mutant.suppression.is_some())
            .map(|mutant| mutant.id.clone())
            .collect()
    };

    let was = suppressed(before);
    let now = suppressed(after);

    Verification {
        missing: intended.iter().filter(|id| !now.contains(*id)).cloned().collect(),
        collateral: now
            .iter()
            .filter(|id| !was.contains(*id) && !intended.contains(*id))
            .cloned()
            .collect(),
    }
}

/// Renders a unified-style diff of one file, for `--dry-run`.
#[must_use]
pub fn diff(path: &Utf8Path, before: &str, after: &str) -> String {
    let mut out = format!("--- {path}\n+++ {path}\n");
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut old_index = 0;

    for line in new {
        if old.get(old_index) == Some(&line) {
            let _ = writeln!(out, " {line}");
            old_index += 1;
        } else {
            let _ = writeln!(out, "+{line}");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use core::iter::once;
    use core::ops::Range;

    use super::*;
    use crate::model::{Channel, Suppression};
    use crate::ops::collect::Shape;

    /// Builds a mutant with the fields this module reads.
    fn mutant(id: &str, file: &str, line: usize, mutator: &str, outcome: Outcome) -> Mutant {
        Mutant {
            id: id.to_owned(),
            ordinal: 1,
            file: Utf8PathBuf::from(file),
            package: "subject".to_owned(),
            span: Range { start: 0, end: 1 },
            line,
            column: 1,
            mutator: mutator.to_owned(),
            item_path: "f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a".to_owned(),
            replacement: "b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    #[test]
    fn a_survivor_is_skipped_even_if_it_reaches_the_planner() {
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 4, "relational.lt_to_le", Outcome::Survived),
            mutant("bbb", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
        ];

        let edits = plan(&mutants, &[Eligible::Timeout]);

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].line, 9);
    }

    #[test]
    fn timeouts_are_eligible_by_default_and_unviables_are_opt_in() {
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 4, "fn_value.default", Outcome::CompileError),
            mutant("bbb", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
        ];

        assert_eq!(plan(&mutants, &[Eligible::Timeout]).len(), 1);
        assert_eq!(plan(&mutants, &[Eligible::Timeout, Eligible::Unviable]).len(), 2);
    }

    #[test]
    fn ineligible_outcomes_are_not_planned() {
        let mutants = vec![mutant("aaa", "src/lib.rs", 4, "fn_value.default", Outcome::Killed)];

        assert!(plan(&mutants, &[Eligible::Timeout, Eligible::Unviable]).is_empty());
    }

    #[test]
    fn mutators_at_one_site_coalesce_into_a_single_directive() {
        // Five stacked comments above one line is not something anyone keeps.
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
            mutant("bbb", "src/lib.rs", 9, "arith.add_to_sub", Outcome::Timeout),
        ];

        let edits = plan(&mutants, &[Eligible::Timeout]);

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].mutators.len(), 2);
    }

    #[test]
    fn a_directive_is_written_above_the_line_at_its_indentation() {
        let text = "fn f() {\n    loop {}\n}\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines[1].starts_with("    // #[gamma::skip("), "{out}");
        assert_eq!(lines[2], "    loop {}");
    }

    #[test]
    fn edits_for_missing_lines_are_ignored() {
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 99,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        assert_eq!(apply("fn f() {}\n", &[&edit], "2026-08-05"), "fn f() {}\n");
    }

    #[test]
    fn edits_are_applied_from_the_end_so_earlier_lines_stay_valid() {
        // Applying forwards shifts every later line by one and puts the second directive one line
        // too high, which is silent: the file still compiles and suppresses the wrong thing.
        let text = "a();\nb();\nc();\n";
        let first = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 1,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };
        let second = Edit { line: 3, ..first.clone() };

        let out = apply(text, &[&first, &second], "2026-08-05");
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines[0].contains("gamma::skip"), "{out}");
        assert_eq!(lines[1], "a();");
        assert_eq!(lines[2], "b();");
        assert!(lines[3].contains("gamma::skip"), "{out}");
        assert_eq!(lines[4], "c();");
    }

    #[test]
    fn running_twice_is_a_no_op() {
        let text = "fn f() {\n    loop {}\n}\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let once = apply(text, &[&edit], "2026-08-05");

        // The second pass sees the directive it wrote, so the line it targets has moved down by one.
        let again = apply(&once, &[&Edit { line: 3, ..edit }], "2026-08-05");

        assert_eq!(once, again);
        assert_eq!(again.matches("gamma::skip").count(), 1, "{again}");
    }

    #[test]
    fn a_second_mutator_extends_the_generated_directive_rather_than_stacking() {
        let text = "fn f() {\n    loop {}\n}\n";
        let first = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let once = apply(text, &[&first], "2026-08-05");
        let second = Edit {
            line: 3,
            mutators: core::iter::once("arith.add_to_sub".to_owned()).collect(),
            ..first
        };
        let twice = apply(&once, &[&second], "2026-08-05");

        assert_eq!(twice.matches("gamma::skip").count(), 1, "{twice}");
        assert!(twice.contains("arith.add_to_sub"), "{twice}");
        assert!(twice.contains("stmt.delete"), "{twice}");
    }

    #[test]
    fn a_hand_written_directive_is_never_rewritten() {
        // Someone's reason is the most valuable thing in the file, and it is not recoverable.
        let text = "fn f() {\n    // #[gamma::skip(stmt.delete, reason = \"driver poll, see RFC-12\")]\n    loop {}\n}\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 3,
            mutators: once("arith.add_to_sub".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");

        assert!(out.contains("RFC-12"), "{out}");
        assert_eq!(out.matches("gamma::skip").count(), 2, "{out}");
    }

    #[test]
    fn verification_notices_an_edit_that_suppressed_nothing() {
        let before = vec![mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout)];
        let after = before.clone();
        let intended: BTreeSet<String> = once("aaa".to_owned()).collect();

        let result = verify(&before, &after, &intended);

        assert!(!result.is_clean());
        assert_eq!(result.missing, vec!["aaa".to_owned()]);
    }

    #[test]
    fn verification_notices_collateral_suppression() {
        // The hazard the whole design is arranged around: a directive on a multi-line construct
        // takes out everything inside it, which can include survivors.
        let before = vec![
            mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
            mutant("bbb", "src/lib.rs", 10, "arith.add_to_sub", Outcome::Survived),
        ];
        let mut after = before.clone();

        for entry in &mut after {
            entry.suppression = Some(Suppression {
                channel: Channel::Comment,
                reason: None,
                tag: None,
                line: Some(8),
            });
        }

        let intended: BTreeSet<String> = once("aaa".to_owned()).collect();
        let result = verify(&before, &after, &intended);

        assert!(!result.is_clean());
        assert_eq!(result.collateral, vec!["bbb".to_owned()]);
    }

    #[test]
    fn a_clean_verification_is_both_halves() {
        let before = vec![mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout)];
        let mut after = before.clone();

        after[0].suppression = Some(Suppression {
            channel: Channel::Comment,
            reason: None,
            tag: None,
            line: Some(8),
        });

        let intended: BTreeSet<String> = once("aaa".to_owned()).collect();

        assert!(verify(&before, &after, &intended).is_clean());
    }

    #[test]
    fn the_epoch_converts_to_its_known_date() {
        // Two fixed points, one of them a leap day, because the whole algorithm is about where the
        // leap day lands.
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_417), "2023-03-01");
        assert_eq!(civil_from_days(18_321), "2020-02-29");
    }

    #[test]
    fn today_is_a_plausible_date() {
        let date = today();

        assert_eq!(date.len(), 10, "{date}");
        assert!(date.starts_with("20"), "{date}");
    }

    #[test]
    fn the_diff_marks_only_the_added_lines() {
        let before = "a();\nb();\n";
        let after = "a();\n// added\nb();\n";

        let text = diff(Utf8Path::new("src/lib.rs"), before, after);

        assert!(text.contains("+// added"), "{text}");
        assert!(text.contains(" a();"), "{text}");
        let added = text.lines().skip(2).filter(|line| line.starts_with('+')).count();

        assert_eq!(added, 1, "{text}");
    }

    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        // A lone LF in an otherwise CRLF file is a whitespace change on a line nobody edited, which
        // is exactly the kind of diff that makes a team stop trusting an automated fix.
        let text = "fn f() {\r\n    loop {}\r\n}\r\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");

        assert_eq!(out.matches('\n').count(), out.matches("\r\n").count(), "{out:?}");
        assert!(out.contains("    // #[gamma::skip(stmt.delete,"), "{out:?}");
    }

    #[test]
    fn an_lf_file_keeps_its_line_endings_even_with_one_stray_crlf() {
        // Majority rather than first-seen: a file with one stray ending still has a convention, and
        // matching the stray one would spread it.
        let text = "fn f() {\n    loop {}\r\n}\n\n\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");

        assert!(out.contains("suppress 2026-08-05\")]\n    loop"), "{out:?}");
    }
}
