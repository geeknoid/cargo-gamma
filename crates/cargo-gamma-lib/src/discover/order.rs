//! Ordering workspace packages so that a package is processed after everything it depends on.

/// A set of packages that have to be processed together, because each can reach the others.
///
/// Almost every stage holds a single package. A stage holds more than one only when packages are
/// mutually reachable, which happens when a dev-dependency closes a cycle: `a` depends on `b`, and
/// `b`'s integration tests depend on `a`. Neither can be placed before the other, so neither is.
pub type Stage = Vec<String>;

/// Orders packages so that every package follows the packages it depends on.
///
/// The order comes from the sizes of the reach sets rather than from a traversal. `reach` maps a
/// package to itself plus everything it can reach, so a dependency's reach set is a strict subset
/// of its dependent's, and sorting by size ascending is therefore a topological order. Packages
/// with equal reach sets are mutually reachable and share a stage. Ties are broken by name, so the
/// same workspace always produces the same order.
pub fn stages(packages: &[String], reach: &crate::HashMap<String, crate::HashSet<String>>) -> Vec<Stage> {
    let mut ordered: Vec<&String> = packages.iter().collect();
    let size = |name: &str| reach.get(name).map_or(0, crate::HashSet::len);

    ordered.sort_by(|left, right| size(left).cmp(&size(right)).then_with(|| left.cmp(right)));

    let mut stages: Vec<Stage> = Vec::new();

    for name in ordered {
        // Only the previous stage needs checking: equal reach sets sort adjacently, so a package
        // mutually reachable with an earlier one is always next to it.
        let joins = stages.last().and_then(|stage| stage.first()).is_some_and(|first| {
            reach.get(first.as_str()).is_some_and(|theirs| {
                reach.get(name.as_str()).is_some_and(|mine| mine == theirs)
            })
        });

        if joins {
            if let Some(stage) = stages.last_mut() {
                stage.push(name.clone());
            }
        } else {
            stages.push(vec![name.clone()]);
        }
    }

    stages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reach(entries: &[(&str, &[&str])]) -> crate::HashMap<String, crate::HashSet<String>> {
        entries
            .iter()
            .map(|(name, reaches)| {
                (
                    (*name).to_owned(),
                    reaches.iter().map(|entry| (*entry).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_dependency_is_processed_before_its_dependent() {
        let reach = reach(&[("leaf", &["leaf"]), ("mid", &["mid", "leaf"]), ("top", &["top", "mid", "leaf"])]);
        let packages = vec!["top".to_owned(), "leaf".to_owned(), "mid".to_owned()];

        let stages = stages(&packages, &reach);

        assert_eq!(stages, vec![vec!["leaf"], vec!["mid"], vec!["top"]]);
    }

    #[test]
    fn independent_packages_are_ordered_by_name() {
        // Neither can depend on the other, so the only thing left to order them by is their names,
        // and the order has to be the same on every run.
        let reach = reach(&[("beta", &["beta"]), ("alpha", &["alpha"])]);
        let packages = vec!["beta".to_owned(), "alpha".to_owned()];

        assert_eq!(stages(&packages, &reach), vec![vec!["alpha"], vec!["beta"]]);
    }

    #[test]
    fn mutually_reachable_packages_share_a_stage() {
        // `b`'s tests depend on `a`, which depends on `b`. Neither can be built first, so both are
        // built at once rather than one being placed arbitrarily ahead of the other.
        let reach = reach(&[("a", &["a", "b"]), ("b", &["a", "b"])]);
        let packages = vec!["b".to_owned(), "a".to_owned()];

        assert_eq!(stages(&packages, &reach), vec![vec!["a", "b"]]);
    }

    #[test]
    fn a_package_missing_from_the_reach_map_still_appears() {
        // Losing a package here would mean never scanning or building it, which would silently
        // drop every mutant it holds.
        let stages = stages(&["orphan".to_owned()], &reach(&[]));

        assert_eq!(stages, vec![vec!["orphan"]]);
    }

    #[test]
    fn packages_are_never_lost_or_duplicated() {
        let reach = reach(&[("leaf", &["leaf"]), ("mid", &["mid", "leaf"]), ("other", &["other", "leaf"])]);
        let packages = vec!["mid".to_owned(), "other".to_owned(), "leaf".to_owned()];

        let flattened: Vec<String> = stages(&packages, &reach).into_iter().flatten().collect();

        assert_eq!(flattened.len(), packages.len());

        for package in &packages {
            assert!(flattened.contains(package), "{package} was dropped");
        }
    }
}
