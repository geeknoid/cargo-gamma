//! Working out which Cargo features are on, without asking Cargo to resolve the whole graph.
//!
//! `#[cfg(feature = "...")]` is the most common gate in a real workspace, and `rustc` cannot answer
//! it: features are Cargo's concept, not the compiler's. The obvious source is `cargo metadata`
//! with its dependency resolution, but that requires a resolvable graph and can reach the network,
//! which would make discovery fail on a tree that builds perfectly well offline.
//!
//! So the answer is computed here from the metadata this tool already loads, which lists every
//! workspace member's own feature table and its declared dependencies. That is enough to be exact
//! about workspace members, which are the only packages whose code gets mutated.
//!
//! # What is resolved
//!
//! Starting from what the command line selected, three things propagate to a fixed point:
//!
//! 1. A feature's own entries, so `default = ["std"]` turns `default` on into `std` on.
//! 2. A `member/feature` entry, so one member can turn on another member's feature.
//! 3. A dependency declaration on another member, which contributes its `features` list and, unless
//!    it opted out, that member's `default`.
//!
//! Development and build dependencies count too, because the schema is built with `cargo test`,
//! which compiles all of them.
//!
//! # Erring toward keeping a mutant
//!
//! Anything ambiguous resolves toward the feature being *on*, which keeps the code mutable and the
//! mutants in the population. An optional dependency is treated as enabled if anything could enable
//! it, because being wrong in the other direction would silently drop mutants from live code.

use crate::commands::FeatureArgs;
use crate::{HashMap, HashSet};
use cargo_metadata::{DependencyKind, Metadata, Package};

/// Returns the features enabled for each workspace member.
///
/// Only workspace members appear: a registry dependency is never mutated, so its features are of no
/// interest, and guessing at them would cost a full resolve.
///
/// ```rust,no_run
/// # use cargo_gamma_lib::cfg::features::enabled;
/// # use cargo_gamma_lib::commands::FeatureArgs;
/// # fn example(metadata: &cargo_metadata::Metadata) {
/// let features = enabled(metadata, &FeatureArgs::default());
///
/// // Every member is present, even one with no features at all.
/// assert!(features.contains_key("my-crate"));
/// # }
/// ```
#[must_use]
pub fn enabled(metadata: &Metadata, args: &FeatureArgs) -> HashMap<String, Vec<String>> {
    let members: Vec<&Package> = metadata.workspace_packages();
    let named = requested(args);
    let mut on: HashMap<String, HashSet<String>> = HashMap::default();

    for package in &members {
        let _old = on.insert(package.name.as_str().to_owned(), seed(package, args, &named));
    }

    // Propagation is monotone — nothing is ever turned off — so iterating until a pass adds
    // nothing terminates, and the bound is the total number of features in the workspace.
    let mut budget = members.iter().map(|package| package.features.len()).sum::<usize>() + members.len();

    while budget > 0 && propagate(&members, &mut on) {
        budget -= 1;
    }

    on.into_iter()
        .map(|(package, features)| {
            let mut sorted: Vec<String> = features.into_iter().collect();

            sorted.sort();

            (package, sorted)
        })
        .collect()
}

/// Splits the `--features` values into the flat list of names they denote.
///
/// Cargo accepts both `--features a,b` and `--features a --features b`, and a `package/feature`
/// entry names a feature of another package.
fn requested(args: &FeatureArgs) -> Vec<(Option<String>, String)> {
    args.features
        .iter()
        .flat_map(|entry| entry.split([',', ' ']))
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('/') {
            Some((package, feature)) => (Some(package.to_owned()), feature.to_owned()),
            None => (None, entry.to_owned()),
        })
        .collect()
}

/// Returns the features a package starts with, before anything propagates.
fn seed(package: &Package, args: &FeatureArgs, named: &[(Option<String>, String)]) -> HashSet<String> {
    let mut on: HashSet<String> = HashSet::default();

    if args.all_features {
        on.extend(package.features.keys().cloned());

        return on;
    }

    if !args.no_default_features && package.features.contains_key("default") {
        let _added = on.insert("default".to_owned());
    }

    for (owner, feature) in named {
        // An unqualified name applies to whichever selected packages declare it, which is what
        // Cargo does for a workspace build. A qualified one names its package outright.
        let mine = owner.as_ref().is_none_or(|owner| owner == package.name.as_str());

        if mine && package.features.contains_key(feature) {
            let _added = on.insert(feature.clone());
        }
    }

    on
}

/// Runs one propagation pass, returning whether anything changed.
fn propagate(members: &[&Package], on: &mut HashMap<String, HashSet<String>>) -> bool {
    let mut changed = false;

    for package in members {
        let name = package.name.as_str();
        let mine = on.get(name).cloned().unwrap_or_default();

        // A feature's own entries.
        for feature in &mine {
            let Some(entries) = package.features.get(feature) else {
                continue;
            };

            for entry in entries {
                changed |= apply(entry, name, on);
            }
        }

        // A dependency on another member contributes what the declaration asks for. Optional
        // dependencies are included: deciding they are off would drop mutants from code that a
        // feature elsewhere may well have switched on.
        for dependency in &package.dependencies {
            let target = dependency.rename.as_deref().map_or(dependency.name.as_str(), |_| dependency.name.as_str());

            if !on.contains_key(target) {
                continue;
            }

            if matches!(dependency.kind, DependencyKind::Unknown) {
                continue;
            }

            for feature in &dependency.features {
                changed |= turn_on(target, feature, on);
            }

            if dependency.uses_default_features {
                changed |= turn_on(target, "default", on);
            }
        }
    }

    changed
}

/// Applies one entry from a feature table, which may name this package's feature or another's.
///
/// The spellings are `plain`, `dep:some-crate`, `some-crate/feature` and `some-crate?/feature`.
/// Only the ones that name a feature matter here; `dep:` merely activates an optional dependency,
/// which this module already assumes.
fn apply(entry: &str, owner: &str, on: &mut HashMap<String, HashSet<String>>) -> bool {
    if entry.starts_with("dep:") {
        return false;
    }

    match entry.split_once('/') {
        Some((package, feature)) => turn_on(package.trim_end_matches('?'), feature, on),
        None => turn_on(owner, entry, on),
    }
}

/// Turns a feature on for a package, returning whether that was news.
///
/// A package that is not a workspace member is ignored: its code is never mutated, so what it
/// compiles is not this tool's business.
fn turn_on(package: &str, feature: &str, on: &mut HashMap<String, HashSet<String>>) -> bool {
    on.get_mut(package).is_some_and(|features| features.insert(feature.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::load_metadata;
    use camino::Utf8PathBuf;
    use std::fs;
    use tempfile::TempDir;

    /// Writes a workspace of manifests and gives back the metadata cargo reads from it.
    ///
    /// A `src/lib.rs` is written beside every manifest that declares a package, because a package
    /// with no target at all is not something cargo will describe.
    fn metadata_for(files: &[(&str, &str)]) -> (TempDir, Metadata) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        for (relative, contents) in files {
            let path = root.join(relative);

            fs::create_dir_all(path.parent().expect("a manifest has a directory").as_std_path()).expect("directories");
            fs::write(path.as_std_path(), contents).expect("the manifest is written");

            if contents.contains("[package]") {
                let source = path.parent().expect("a manifest has a directory").join("src");

                fs::create_dir_all(source.as_std_path()).expect("a source directory");
                fs::write(source.join("lib.rs").as_std_path(), "pub fn f() {}\n").expect("a library root");
            }
        }

        let metadata = load_metadata(&root, &FeatureArgs::default()).expect("the fixture workspace has metadata");

        (directory, metadata)
    }

    /// The features enabled for `package` under `args`, as a sorted list.
    fn features_of(files: &[(&str, &str)], args: &FeatureArgs, package: &str) -> Vec<String> {
        let (_directory, metadata) = metadata_for(files);

        enabled(&metadata, args).remove(package).unwrap_or_default()
    }

    /// A single-package workspace whose library declares the given feature table.
    fn alone(table: &str) -> String {
        format!("[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\n{table}\n\n[workspace]\n")
    }

    #[test]
    fn default_is_on_and_expands() {
        let manifest = alone("default = [\"std\"]\nstd = []\nstats = []\n");
        let found = features_of(&[("Cargo.toml", &manifest)], &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["default".to_owned(), "std".to_owned()]);
    }

    #[test]
    fn no_default_features_leaves_nothing_on() {
        let manifest = alone("default = [\"std\"]\nstd = []\n");
        let args = FeatureArgs { no_default_features: true, ..FeatureArgs::default() };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").is_empty());
    }

    #[test]
    fn all_features_turns_on_everything_declared() {
        let manifest = alone("default = [\"std\"]\nstd = []\nstats = []\n");
        let args = FeatureArgs { all_features: true, ..FeatureArgs::default() };
        let found = features_of(&[("Cargo.toml", &manifest)], &args, "alpha");

        assert_eq!(found, vec!["default".to_owned(), "stats".to_owned(), "std".to_owned()]);
    }

    #[test]
    fn a_named_feature_is_turned_on() {
        let manifest = alone("default = [\"std\"]\nstd = []\nstats = []\n");
        let args = FeatureArgs { features: vec!["stats".to_owned()], ..FeatureArgs::default() };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").contains(&"stats".to_owned()));
    }

    #[test]
    fn several_named_features_may_share_one_argument() {
        // Cargo accepts `--features a,b`, so a value that was never split would name no feature at
        // all and quietly leave both off.
        let manifest = alone("a = []\nb = []\nc = []\n");
        let args = FeatureArgs { features: vec!["a,b".to_owned()], ..FeatureArgs::default() };
        let found = features_of(&[("Cargo.toml", &manifest)], &args, "alpha");

        assert_eq!(found, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn a_feature_that_is_not_declared_is_not_invented() {
        let manifest = alone("a = []\n");
        let args = FeatureArgs { features: vec!["nope".to_owned()], ..FeatureArgs::default() };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").is_empty());
    }

    #[test]
    fn a_qualified_name_only_reaches_its_own_package() {
        let manifest = alone("a = []\n");
        let args = FeatureArgs { features: vec!["beta/a".to_owned()], ..FeatureArgs::default() };

        assert!(features_of(&[("Cargo.toml", &manifest)], &args, "alpha").is_empty());
    }

    #[test]
    fn features_chain_through_their_own_entries() {
        let manifest = alone("default = [\"a\"]\na = [\"b\"]\nb = [\"c\"]\nc = []\n");
        let found = features_of(&[("Cargo.toml", &manifest)], &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["a".to_owned(), "b".to_owned(), "c".to_owned(), "default".to_owned()]);
    }

    #[test]
    fn a_cycle_between_features_terminates() {
        // A malformed manifest must not hang discovery.
        let manifest = alone("default = [\"a\"]\na = [\"b\"]\nb = [\"a\"]\n");
        let found = features_of(&[("Cargo.toml", &manifest)], &FeatureArgs::default(), "alpha");

        assert!(found.contains(&"a".to_owned()), "{found:?}");
        assert!(found.contains(&"b".to_owned()), "{found:?}");
    }

    #[test]
    fn a_dep_entry_names_no_feature() {
        // `dep:beta` switches on an optional dependency. Reading it as a feature named `dep:beta`
        // would put a name in the set that no `#[cfg]` can ever spell.
        let files = pair(
            "[features]\ndefault = [\"dep:beta\"]\n\n[dependencies]\nbeta = { path = \"../beta\", optional = true, default-features = false }\n",
            "loud = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "alpha");

        assert_eq!(found, vec!["default".to_owned()], "`dep:` activates a crate, not a feature");
        assert!(features_of(&borrowed(&files), &FeatureArgs::default(), "beta").is_empty());
    }

    /// A two-member workspace, where `alpha` relates to `beta` however the caller says.
    fn pair(alpha_extra: &str, beta_features: &str) -> Vec<(String, String)> {
        vec![
            ("Cargo.toml".to_owned(), "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"3\"\n".to_owned()),
            (
                "alpha/Cargo.toml".to_owned(),
                format!("[package]\nname = \"alpha\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n{alpha_extra}"),
            ),
            (
                "beta/Cargo.toml".to_owned(),
                format!("[package]\nname = \"beta\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[features]\n{beta_features}"),
            ),
        ]
    }

    /// Borrows an owned fixture into the shape the loader wants.
    fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
        files.iter().map(|(path, text)| (path.as_str(), text.as_str())).collect()
    }

    #[test]
    fn one_member_can_turn_on_anothers_feature() {
        let files = pair(
            "[features]\ndefault = [\"beta/loud\"]\n\n[dependencies]\nbeta = { path = \"../beta\", default-features = false }\n",
            "loud = []\nquiet = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert_eq!(found, vec!["loud".to_owned()]);
    }

    #[test]
    fn a_dependency_declaration_contributes_its_features() {
        let files = pair(
            "[dependencies]\nbeta = { path = \"../beta\", features = [\"loud\"] }\n",
            "default = [\"quiet\"]\nloud = []\nquiet = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert!(found.contains(&"loud".to_owned()), "{found:?}");
        assert!(found.contains(&"quiet".to_owned()), "the default came through too: {found:?}");
    }

    #[test]
    fn a_dev_dependency_counts_because_the_schema_is_built_with_cargo_test() {
        let files = pair(
            "[dev-dependencies]\nbeta = { path = \"../beta\", features = [\"loud\"], default-features = false }\n",
            "loud = []\nquiet = []\n",
        );

        let found = features_of(&borrowed(&files), &FeatureArgs::default(), "beta");

        assert_eq!(found, vec!["loud".to_owned()]);
    }

    #[test]
    fn a_feature_of_a_package_outside_the_workspace_is_ignored() {
        // Only members are mutated, so what a dependency outside the workspace compiles is not
        // this tool's business, and naming one must not invent an entry for it.
        let manifest = alone("default = [\"a\"]\na = []\n");
        let (_directory, metadata) = metadata_for(&[("Cargo.toml", &manifest)]);
        let found = enabled(&metadata, &FeatureArgs::default());

        assert_eq!(found.len(), 1, "only the one member is described: {found:?}");
    }

    #[test]
    fn every_member_appears_even_with_no_features() {
        let manifest = alone("");
        let (_directory, metadata) = metadata_for(&[("Cargo.toml", &manifest)]);
        let found = enabled(&metadata, &FeatureArgs::default());

        assert!(found.contains_key("alpha"), "a missing package would be left unconditional");
        assert!(found["alpha"].is_empty());
    }
}
