//! Deleting whole directive lines from a file's text.

use std::collections::BTreeSet;

/// Deletes whole lines from a file's text, by one-based line number.
///
/// The counterpart of [`apply`], and deliberately the dumbest thing that can work: a directive is
/// only ever removed when it is the entire content of its own line, so removal is a line delete and
/// nothing else. See [`removable`] for what makes that true, and why anything else is left alone.
#[must_use]
pub fn remove(text: &str, lines: &BTreeSet<usize>) -> String {
    text.split_inclusive('\n')
        .enumerate()
        .filter(|(index, _)| !lines.contains(&(index + 1)))
        .map(|(_, line)| line)
        .collect()
}

/// Whether a line holds a skip directive and nothing else, so that deleting the line deletes it.
///
/// Removal has to be conservative in a way that adding does not. A directive can be attached to a
/// line of code, wrapped in a `cfg_attr`, or spread over several lines, and in each of those cases
/// there is no line whose deletion removes the directive and only the directive. Editing *within* a
/// line to take one attribute out of a list is a different and much less safe operation, so those
/// are reported and left for a person.
#[must_use]
pub fn removable(line: &str) -> bool {
    let trimmed = line.trim();

    // The comment forms — `// gamma::skip(…)` and `// #[gamma::skip(…)]` — are the same directive
    // with different decoration, and both are the whole line by construction.
    let body = trimmed.strip_prefix("//").map_or(trimmed, str::trim_start);
    let body = body.strip_prefix("#[").and_then(|rest| rest.strip_suffix(']')).unwrap_or(body);

    if !body.starts_with("gamma::skip") {
        return false;
    }

    // An attribute whose arguments run onto the next line leaves its parentheses open here, and
    // deleting the first line of it would leave the rest behind as a syntax error.
    let opened = body.matches('(').count();

    opened == body.matches(')').count() && !body.contains("//")
}
