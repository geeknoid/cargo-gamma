//! Reference tables for the documentation, rendered from the registry that defines them.
//!
//! The operator catalog is the tool's public vocabulary: the same names appear on `--ops`, in every
//! suppression directive, in the report, in SARIF rule identifiers and in configuration. A
//! reference that drifts from the registry is therefore worse than no reference at all, because a
//! reader who copies a name out of it gets a usage error and no clue that the document was wrong.
//!
//! So the tables are generated here and checked against the files in `docs/` by a test. Adding a
//! mutator fails that test until the document is regenerated, which is the only arrangement that
//! keeps a hand-written catalog honest as the catalog grows.

use core::fmt::Write as _;

use crate::ops::registry::{PROFILES, REGISTRY, families};

/// The marker that opens a generated block in a documentation file.
///
/// The blocks are delimited rather than owning the whole file so that the prose explaining what a
/// family is *for* can live beside the table listing what it contains. A reference that is only a
/// table tells a reader what exists without telling them when to reach for it.
pub const BEGIN: &str = "<!-- begin generated: ";

/// The marker that closes a generated block.
pub const END: &str = "<!-- end generated -->";

/// Renders the block named `name`, or `None` when no such block exists.
#[must_use]
pub fn block(name: &str) -> Option<String> {
    match name {
        "operators" => Some(operators()),
        "profiles" => Some(profiles()),
        "families" => Some(family_summary()),
        _ => None,
    }
}

/// Every mutator, grouped by family, with its alias and default state.
fn operators() -> String {
    let mut out = String::new();

    for family in families() {
        let members: Vec<_> =
            REGISTRY.iter().filter(|mutator| mutator.name.split('.').next() == Some(family)).collect();

        let _ = writeln!(out, "### `{family}`\n");
        let _ = writeln!(out, "| Mutator | What it does | Alias | Default |");
        let _ = writeln!(out, "| --- | --- | --- | --- |");

        for mutator in members {
            let aliases =
                if mutator.aliases.is_empty() { String::new() } else { format!("`{}`", mutator.aliases.join("`, `")) };

            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                mutator.name,
                escape(mutator.description),
                aliases,
                if mutator.default_on { "yes" } else { "no" }
            );
        }

        out.push('\n');
    }

    out.trim_end().to_owned()
}

/// Every profile, with the selectors it expands to.
fn profiles() -> String {
    let mut out = String::new();

    let _ = writeln!(out, "| Profile | What it selects | Expands to |");
    let _ = writeln!(out, "| --- | --- | --- |");

    for profile in PROFILES {
        let members = profile.members.iter().map(|member| format!("`{member}`")).collect::<Vec<_>>().join(", ");

        let _ = writeln!(out, "| `@{}` | {} | {members} |", profile.name, escape(profile.description));
    }

    out.trim_end().to_owned()
}

/// One row per family, with how many mutators it holds.
fn family_summary() -> String {
    let mut out = String::new();

    let _ = writeln!(out, "| Family | Mutators | What it asks |");
    let _ = writeln!(out, "| --- | ---: | --- |");

    for family in families() {
        let count = REGISTRY.iter().filter(|mutator| mutator.name.split('.').next() == Some(family)).count();

        let _ = writeln!(out, "| [`{family}`](OPERATORS.md#{family}) | {count} | {} |", question(family));
    }

    let _ = writeln!(out, "| **Total** | **{}** | |", REGISTRY.len());

    out.trim_end().to_owned()
}

/// The question a family exists to ask, in the reader's terms rather than the operator's.
///
/// A description of the transform — "replace `<` with `<=`" — says what the tool does, which the
/// per-mutator table already covers. What a reader choosing between families needs is what a
/// survivor in that family would mean about their tests, which is a different sentence.
fn question(family: &str) -> &'static str {
    match family {
        "fn_value" => "Does anything check what this function returns?",
        "relational" => "Is this comparison's boundary the right one?",
        "arith" => "Does this calculation's operator matter?",
        "bitwise" => "Is this mask or flag combination correct?",
        "shift" => "Is this shift's direction load-bearing?",
        "assign" => "Does this compound assignment's operator matter?",
        "assign_value" => "Is the value assigned here ever read in a way that would notice?",
        "logical" => "Is this `&&` really an `&&`?",
        "cond" => "Does anything depend on this branch being taken?",
        "match_guard" => "Does anything depend on this guard being right?",
        "match_arm" => "Is this arm reachable, and does anything notice when it stops matching?",
        "loop" => "Does this `break` or `continue` carry the loop's meaning?",
        "range" => "Is this bound inclusive on purpose?",
        "literal" => "Does this constant's exact value matter?",
        "expr" => "Would an off-by-one here be caught?",
        "unary" => "Does this negation or complement matter?",
        "stmt" => "Does this statement's side effect matter?",
        "struct_field" => "Does this field's value matter, or is the default good enough?",
        "option" => "Is the present case distinguished from the absent one?",
        "result" => "Is success distinguished from failure?",
        "iter" => "Does anything observe that this was ordered, deduplicated, or taken from one end?",
        "string" => "Does the prefix, the case, or the trimmed end actually matter?",
        "collection" => "Does every element of this literal earn its place?",
        _ => "",
    }
}

/// Escapes the characters that would otherwise end a table cell.
fn escape(text: &str) -> String {
    text.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_mutator_appears_in_the_operator_table() {
        // The table is the tool's published vocabulary. A name missing from it is a feature the
        // user cannot discover, and a name in it that the registry does not have is worse: it
        // reads as usable and produces a usage error.
        let rendered = operators();

        for mutator in REGISTRY {
            assert!(rendered.contains(mutator.name), "`{}` is missing from the operator table", mutator.name);
        }
    }

    #[test]
    fn every_family_is_given_a_question_to_ask() {
        // A blank cell in the summary would be the one row a reader skips, and it would be skipped
        // for the newest family — the one most in need of an explanation.
        for family in families() {
            assert!(!question(family).is_empty(), "family `{family}` has no question in the summary table");
        }
    }

    #[test]
    fn a_description_containing_a_pipe_cannot_break_the_table() {
        // `bitwise.or_to_and` and friends describe themselves with `|`, which would otherwise end
        // the cell and silently shift every column after it.
        assert_eq!(escape("replace | with &"), "replace \\| with &");
    }

    #[test]
    fn an_unknown_block_name_is_refused_rather_than_rendered_empty() {
        // A misspelled marker that produced an empty block would delete a whole table from the
        // documentation and pass every check that follows.
        assert!(block("operators").is_some());
        assert!(block("nonesuch").is_none());
    }
}
