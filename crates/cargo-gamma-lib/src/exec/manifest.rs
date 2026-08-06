//! Repairing manifests inside the copied tree.

use camino::{Utf8Path, Utf8PathBuf};
use std::fs;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::Result;
use crate::error::error;

/// The dependency name the instrumented code refers to.
pub(super) const RUNTIME_CRATE: &str = "gamma_rt";

/// Dependency tables, all of which can carry a path.
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// A manifest in the copied tree, edited in place.
///
/// Edits go through `toml_edit` rather than a parse-and-reserialise, so a manifest comes back out
/// with its comments, key order and formatting exactly as the user wrote them. Only the values
/// actually changed are touched.
#[derive(Debug)]
pub(super) struct Manifest {
    path: Utf8PathBuf,
    document: DocumentMut,
    changed: bool,

    /// Where this manifest sits relative to the copied root, which is what decides whether a
    /// dependency path leaves the tree.
    within: Utf8PathBuf,
}

impl Manifest {
    /// Reads a manifest.
    pub(super) fn read(path: &Utf8Path) -> Result<Self> {
        let text = fs::read_to_string(path.as_std_path())
            .map_err(|cause| error!("could not read `{path}`").caused_by(cause))?;

        let document = text
            .parse::<DocumentMut>()
            .map_err(|cause| error!("could not parse `{path}`: {cause}"))?;

        Ok(Self { path: path.to_owned(), document, changed: false, within: Utf8PathBuf::new() })
    }

    /// Writes the manifest back if anything changed.
    pub(super) fn save(&self) -> Result<()> {
        if !self.changed {
            return Ok(());
        }

        fs::write(self.path.as_std_path(), self.document.to_string())
            .map_err(|cause| error!("could not update `{}`", self.path).caused_by(cause))
    }

    /// Rewrites every relative path that leaves the copied tree so that it points back into the
    /// original one.
    ///
    /// A path dependency resolves against the manifest holding it. The copy does not sit where the
    /// original did, so a path leaving the tree — `../../shared` from a package one level down —
    /// lands somewhere that does not exist, and the build fails naming a missing crate rather than
    /// the move that lost it.
    ///
    /// What matters is whether the path leaves the *tree*, not whether it leaves the package. A
    /// sibling dependency written `../core` climbs out of its package but stays well inside the
    /// workspace, and the copy brought the sibling along; re-anchoring it to the original would
    /// make cargo see the same package at two different locations and refuse to write a lockfile.
    ///
    /// `original` is the directory this manifest was copied from, and `within` is that same
    /// directory expressed relative to the copied root — empty for the root manifest itself.
    pub(super) fn anchor_paths(&mut self, original: &Utf8Path, within: &Utf8Path) {
        self.within = within.to_owned();

        for name in DEPENDENCY_TABLES {
            self.anchor_table(name, original);
        }

        // `[replace]` names crates by version requirement, and every entry is a source
        // specification of exactly the same shape as a dependency.
        self.anchor_table("replace", original);

        // Every `[patch.<registry>]` is its own table of dependencies, and a workspace commonly
        // patches a crate to a sibling checkout — the exact shape that breaks.
        if let Some(patch) = self.document.get_mut("patch").and_then(Item::as_table_like_mut) {
            let registries: Vec<String> = patch.iter().map(|(name, _entry)| name.to_owned()).collect();

            for registry in registries {
                if let Some(table) = patch.get_mut(&registry).and_then(Item::as_table_like_mut) {
                    anchor_dependencies(table, original, &self.within, &mut self.changed);
                }
            }
        }

        // A target-specific table holds the same dependency tables one level down.
        if let Some(targets) = self.document.get_mut("target").and_then(Item::as_table_like_mut) {
            let platforms: Vec<String> = targets.iter().map(|(name, _entry)| name.to_owned()).collect();

            for platform in platforms {
                let Some(table) = targets.get_mut(&platform).and_then(Item::as_table_like_mut) else {
                    continue;
                };

                for name in DEPENDENCY_TABLES {
                    if let Some(dependencies) = table.get_mut(name).and_then(Item::as_table_like_mut) {
                        anchor_dependencies(dependencies, original, &self.within, &mut self.changed);
                    }
                }
            }
        }

        // A workspace's own dependency table feeds every member that says `workspace = true`.
        if let Some(workspace) = self.document.get_mut("workspace").and_then(Item::as_table_like_mut)
            && let Some(dependencies) = workspace.get_mut("dependencies").and_then(Item::as_table_like_mut)
        {
            anchor_dependencies(dependencies, original, &self.within, &mut self.changed);
        }
    }

    /// Rewrites the paths in one top-level table.
    fn anchor_table(&mut self, name: &str, original: &Utf8Path) {
        if let Some(table) = self.document.get_mut(name).and_then(Item::as_table_like_mut) {
            anchor_dependencies(table, original, &self.within, &mut self.changed);
        }
    }

    /// Adds the guard runtime as a dependency, unless the package already has it.
    ///
    /// A package may already depend on the runtime — `cargo-gamma`'s own crates do. Two crates
    /// with one library name makes every reference to it ambiguous, so the existing dependency is
    /// left to do the job.
    pub(super) fn link_runtime(&mut self, runtime: &Utf8Path) {
        if self.links_runtime() {
            return;
        }

        let dependencies = self.document.entry("dependencies").or_insert_with(|| Item::Table(Table::new()));

        let Some(table) = dependencies.as_table_like_mut() else {
            return;
        };

        let mut entry = toml_edit::InlineTable::new();
        let _replaced = entry.insert("path", Value::from(runtime.as_str()));

        let _added = table.insert(RUNTIME_CRATE, Item::Value(Value::InlineTable(entry)));

        self.changed = true;
    }

    /// Returns whether the library target can already name the guard runtime.
    ///
    /// Only a normal dependency counts. A dev- or build-dependency is invisible to the lib target,
    /// so treating one as sufficient would leave every guard in library code unable to name the
    /// runtime and fail the build everywhere at once. Declaring the crate in both sections is
    /// legal, so adding ours alongside an existing dev-dependency is safe.
    fn links_runtime(&self) -> bool {
        let named = |table: Option<&Item>| {
            table.and_then(Item::as_table_like).is_some_and(|table| {
                table.contains_key(RUNTIME_CRATE) || table.contains_key("cargo-gamma-rt")
            })
        };

        if named(self.document.get("dependencies")) {
            return true;
        }

        // `[target.'cfg(unix)'.dependencies]` is still a normal dependency table.
        self.document
            .get("target")
            .and_then(Item::as_table_like)
            .is_some_and(|targets| targets.iter().any(|(_platform, entry)| {
                named(entry.as_table_like().and_then(|table| table.get("dependencies")))
            }))
    }
}

/// Rewrites every escaping path in one dependency table.
fn anchor_dependencies(table: &mut dyn toml_edit::TableLike, original: &Utf8Path, within: &Utf8Path, changed: &mut bool) {
    let names: Vec<String> = table.iter().map(|(name, _entry)| name.to_owned()).collect();

    for name in names {
        let Some(entry) = table.get_mut(&name) else {
            continue;
        };

        // A dependency written as a bare version string has no path to fix.
        let Some(specification) = entry.as_table_like_mut() else {
            continue;
        };

        let Some(path) = specification.get("path").and_then(|item| item.as_str()) else {
            continue;
        };

        let Some(anchored) = anchor(path, original, within) else {
            continue;
        };

        let _replaced = specification.insert("path", Item::Value(Value::from(portable_path(&anchored))));

        *changed = true;
    }
}

/// Returns the absolute form of a path that would not survive the move, or `None` if it would.
///
/// A path is left alone when it is already absolute, and when it resolves to somewhere still
/// inside the copied tree — those still resolve in the copy, and rewriting them would tie a tree
/// meant to be self-contained back to the original for no reason.
///
/// `within` is the manifest's directory relative to the copied root, so `within.join(path)` is
/// where the dependency lands relative to that root. Only a path that climbs above it has left.
fn anchor(path: &str, original: &Utf8Path, within: &Utf8Path) -> Option<Utf8PathBuf> {
    let candidate = Utf8Path::new(path);

    if candidate.is_absolute() || !escapes(&within.join(candidate)) {
        return None;
    }

    Some(normalize(&original.join(candidate)))
}

/// Returns whether a relative path ever climbs above the directory it is written in.
fn escapes(path: &Utf8Path) -> bool {
    let mut depth = 0_i32;

    for component in path.components() {
        match component.as_str() {
            "." => {}
            ".." => {
                depth -= 1;

                if depth < 0 {
                    return true;
                }
            }
            _named => depth += 1,
        }
    }

    false
}

/// Resolves `.` and `..` textually.
///
/// The target of a path dependency need not exist yet — a workspace can be assembled in any order
/// — so this cannot go through the filesystem the way canonicalization would. Symlinks are
/// therefore not resolved, which is also what cargo itself does with these paths.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut resolved = Utf8PathBuf::new();

    for component in path.components() {
        match component.as_str() {
            "." => {}
            ".." => {
                if !resolved.pop() {
                    resolved.push("..");
                }
            }
            named => resolved.push(named),
        }
    }

    resolved
}

/// Cargo paths are portable when written with `/`, including on Windows.
fn portable_path(path: &Utf8Path) -> String {
    path.as_str().replace('\\', "/")
}

/// Rewrites the `paths` overrides in a `.cargo/config.toml`, if there is one.
///
/// These are relative to the directory holding `.cargo`, and break in exactly the way a path
/// dependency does. A missing or unparseable file is not an error: cargo tolerates the absence,
/// and a file this tool cannot read is one the user's own build is already failing on.
pub(super) fn anchor_cargo_config(root: &Utf8Path, original: &Utf8Path) -> Result<()> {
    for name in ["config.toml", "config"] {
        let path = root.join(".cargo").join(name);

        if !path.as_std_path().is_file() {
            continue;
        }

        let mut manifest = Manifest::read(&path)?;

        if let Some(paths) = manifest.document.get_mut("paths").and_then(Item::as_array_mut) {
            for entry in paths.iter_mut() {
                let Some(anchored) = entry.as_str().and_then(|path| anchor(path, original, Utf8Path::new(""))) else {
                    continue;
                };

                *entry = Value::from(portable_path(&anchored));
                manifest.changed = true;
            }
        }

        manifest.save()?;
    }

    Ok(())
}

/// The flag the instrumented tree is built with, so that the user's lint levels do not judge it.
pub(super) const CAP_LINTS: &str = "--cap-lints=allow";

/// Adds [`CAP_LINTS`] to whatever rustflags the copied tree already configures.
///
/// Setting `RUSTFLAGS` in the environment would be simpler, but the environment variable *replaces*
/// the configured flags rather than adding to them: a workspace whose `.cargo/config.toml` sets
/// `target.<triple>.rustflags` would build with none of them, which can change what its code
/// compiles to and therefore what its tests prove.
///
/// The flag is appended to every rustflags key already present, because cargo picks exactly one of
/// them — `target.<triple>` over `target.<cfg>` over `build` — and which one is not knowable here
/// without resolving the target triple. Appending to all of them means the winner carries the flag
/// whichever it turns out to be. If none is configured, `build.rustflags` is created.
pub(super) fn cap_lints(root: &Utf8Path) -> Result<()> {
    let path = root.join(".cargo").join("config.toml");
    let legacy = root.join(".cargo").join("config");
    let path = if !path.as_std_path().is_file() && legacy.as_std_path().is_file() { legacy } else { path };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path())
            .map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
    }

    if !path.as_std_path().is_file() {
        fs::write(path.as_std_path(), format!("[build]\nrustflags = [\"{CAP_LINTS}\"]\n"))
            .map_err(|cause| error!("could not write `{path}`").caused_by(cause))?;

        return Ok(());
    }

    let mut manifest = Manifest::read(&path)?;
    let mut found = false;

    if let Some(build) = manifest.document.get_mut("build").and_then(Item::as_table_like_mut)
        && let Some(flags) = build.get_mut("rustflags")
    {
        found |= append_flag(flags);
    }

    if let Some(targets) = manifest.document.get_mut("target").and_then(Item::as_table_like_mut) {
        for (_name, entry) in targets.iter_mut() {
            let Some(table) = entry.as_table_like_mut() else {
                continue;
            };

            if let Some(flags) = table.get_mut("rustflags") {
                found |= append_flag(flags);
            }
        }
    }

    if !found {
        manifest.document["build"]["rustflags"] = Item::Value(Value::Array(core::iter::once(CAP_LINTS).collect()));
    }

    manifest.changed = true;
    manifest.save()
}

/// Appends the cap to one rustflags entry, which cargo accepts as an array or as one string.
fn append_flag(flags: &mut Item) -> bool {
    if let Some(array) = flags.as_array_mut() {
        array.push(CAP_LINTS);

        return true;
    }

    if let Some(text) = flags.as_str() {
        *flags = Item::Value(Value::from(format!("{text} {CAP_LINTS}")));

        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(text: &str, original: &str) -> String {
        within(text, original, "")
    }

    fn within(text: &str, original: &str, within: &str) -> String {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), text).unwrap();

        let mut manifest = Manifest::read(&path).unwrap();

        manifest.anchor_paths(Utf8Path::new(original), Utf8Path::new(within));

        manifest.document.to_string()
    }

    #[test]
    fn a_dependency_written_as_a_bare_version_is_left_alone() {
        // `serde = "1"` has no path to anchor, and treating the string as a specification table
        // would either panic or silently rewrite the version requirement.
        let text = "[dependencies]\nserde = \"1\"\ncore = { path = \"../core\" }\n";
        let fixed = fixed(text, "/src/app");

        assert!(fixed.contains("serde = \"1\""), "{fixed}");
        assert!(fixed.contains("/src/core"), "{fixed}");
    }

    #[test]
    fn a_sibling_inside_the_copied_tree_is_left_alone() {
        // `app/../core` is `core`, which the copy brought along. Anchoring it back to the original
        // would put the same package at two locations and cargo would refuse to write a lockfile.
        let fixed = within("[dependencies]\ncore = { path = \"../core\" }\n", "/src/app", "app");

        assert!(fixed.contains("path = \"../core\""), "{fixed}");
    }

    #[test]
    fn a_path_leaving_the_copied_tree_is_anchored_even_from_a_nested_package() {
        let fixed = within("[dependencies]\nshared = { path = \"../../shared\" }\n", "/src/work/app", "app");

        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn a_path_leaving_the_package_is_anchored() {
        let fixed = fixed("[dependencies]\nshared = { path = \"../shared\" }\n", "/src/app");

        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn a_path_staying_inside_the_package_is_left_alone() {
        // It still resolves in the copy, and rewriting it would tie the tree back to the original.
        let fixed = fixed("[dependencies]\ninner = { path = \"crates/inner\" }\n", "/src/app");

        assert!(fixed.contains("path = \"crates/inner\""), "{fixed}");
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        let fixed = fixed("[dependencies]\nshared = { path = \"/elsewhere/shared\" }\n", "/src/app");

        assert!(fixed.contains("path = \"/elsewhere/shared\""), "{fixed}");
    }

    #[test]
    fn a_path_that_descends_before_climbing_is_judged_on_the_whole_journey() {
        // `crates/../../shared` leaves the package even though it starts by entering it.
        let fixed = fixed("[dependencies]\nshared = { path = \"crates/../../shared\" }\n", "/src/app");

        assert!(fixed.contains("path = \"/src/shared\""), "{fixed}");
    }

    #[test]
    fn every_kind_of_dependency_table_is_covered() {
        let text = "[dependencies]\na = { path = \"../a\" }\n\
                    [dev-dependencies]\nb = { path = \"../b\" }\n\
                    [build-dependencies]\nc = { path = \"../c\" }\n\
                    [target.'cfg(unix)'.dependencies]\nd = { path = \"../d\" }\n\
                    [patch.crates-io]\ne = { path = \"../e\" }\n\
                    [workspace.dependencies]\nf = { path = \"../f\" }\n";

        let fixed = fixed(text, "/src/app");

        for crate_name in ["a", "b", "c", "d", "e", "f"] {
            assert!(fixed.contains(&format!("path = \"/src/{crate_name}\"")), "{crate_name} in {fixed}");
        }
    }

    #[test]
    fn comments_and_formatting_survive() {
        // The whole reason for editing rather than reserialising.
        let text = "# keep me\n[dependencies]\n# and me\nshared = { path = \"../shared\" }  # trailing\n";
        let fixed = fixed(text, "/src/app");

        assert!(fixed.contains("# keep me"), "{fixed}");
        assert!(fixed.contains("# and me"), "{fixed}");
        assert!(fixed.contains("# trailing"), "{fixed}");
    }

    #[test]
    fn a_version_only_dependency_is_untouched() {
        let fixed = fixed("[dependencies]\nserde = \"1\"\n", "/src/app");

        assert!(fixed.contains("serde = \"1\""), "{fixed}");
    }

    #[test]
    fn target_entries_that_are_not_tables_are_skipped() {
        let fixed = fixed("[target]\nnot_a_table = \"ignored\"\n[dependencies]\na = { path = \"../a\" }\n", "/src/app");

        // Some manifests put metadata under target-like tables; non-tables must not stop ordinary
        // dependencies later in the document from being repaired.
        assert!(fixed.contains("not_a_table = \"ignored\""), "{fixed}");
        assert!(fixed.contains("path = \"/src/a\""), "{fixed}");
    }

    #[test]
    fn dependencies_without_a_path_are_skipped() {
        let fixed = fixed("[dependencies]\nserde = { version = \"1\" }\nlocal = { path = \"../local\" }\n", "/src/app");

        // Versioned inline-table dependencies are pathless and should survive byte-for-byte while
        // path dependencies next to them are still anchored.
        assert!(fixed.contains("serde = { version = \"1\" }"), "{fixed}");
        assert!(fixed.contains("path = \"/src/local\""), "{fixed}");
    }

    fn linked(text: &str) -> String {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("Cargo.toml")).unwrap();

        fs::write(path.as_std_path(), text).unwrap();

        let mut manifest = Manifest::read(&path).unwrap();

        manifest.link_runtime(Utf8Path::new("/scratch/rt"));

        manifest.document.to_string()
    }

    #[test]
    fn the_runtime_is_added_to_a_package_without_it() {
        let text = linked("[package]\nname = \"x\"\n");

        assert!(text.contains("gamma_rt"), "{text}");
        assert!(text.contains("/scratch/rt"), "{text}");
    }

    #[test]
    fn the_runtime_is_added_to_an_existing_dependency_table() {
        let text = linked("[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n");

        assert!(text.contains("gamma_rt"), "{text}");
        assert!(text.contains("serde = \"1\""), "{text}");
    }

    #[test]
    fn a_malformed_dependency_table_cannot_receive_the_runtime() {
        let text = linked("dependencies = \"not a table\"\n[package]\nname = \"x\"\n");

        // If the user wrote a non-table where dependencies belong, this pass leaves it to cargo's
        // manifest parser rather than inventing a structure and hiding the original problem.
        assert!(!text.contains("gamma_rt"), "{text}");
        assert!(text.contains("dependencies = \"not a table\""), "{text}");
    }

    #[test]
    fn an_existing_runtime_dependency_is_not_duplicated() {
        // Two crates with one library name makes every guard call ambiguous.
        for text in [
            "[dependencies]\ncargo-gamma-rt = { workspace = true }\n",
            "[dependencies]\ngamma_rt = { path = \"../rt\" }\n",
            "[dependencies.gamma_rt]\npath = \"../rt\"\n",
            "[target.'cfg(unix)'.dependencies]\ngamma_rt = \"1\"\n",
        ] {
            let linked = linked(text);

            assert_eq!(linked.matches("gamma_rt").count() + linked.matches("cargo-gamma-rt").count(), 1, "{linked}");
        }
    }

    #[test]
    fn a_dependency_the_library_target_cannot_see_does_not_count() {
        // Guards live in library code, where a dev- or build-dependency is not in scope.
        for text in [
            "[dev-dependencies]\ngamma_rt = \"1\"\n",
            "[build-dependencies]\ngamma_rt = \"1\"\n",
        ] {
            assert!(linked(text).contains("/scratch/rt"), "{text}");
        }
    }

    #[test]
    fn a_cargo_config_path_override_is_anchored() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        fs::create_dir_all(root.join(".cargo").as_std_path()).unwrap();
        fs::write(
            root.join(".cargo").join("config.toml").as_std_path(),
            "paths = [\"../vendored\", \"inside\"]\n",
        )
        .unwrap();

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).unwrap();

        let text = fs::read_to_string(root.join(".cargo").join("config.toml").as_std_path()).unwrap();

        assert!(text.contains("/src/vendored"), "{text}");
        assert!(text.contains("\"inside\""), "{text}");
    }

    #[test]
    fn the_lint_cap_is_added_to_configured_rustflags_rather_than_replacing_them() {
        // Setting `RUSTFLAGS` would drop these, which can change what the tree compiles to and
        // therefore what its tests prove.
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().unwrap().as_std_path()).unwrap();
        fs::write(
            config.as_std_path(),
            "[build]\nrustflags = [\"--cfg\", \"loom\"]\n\n[target.x86_64-unknown-linux-gnu]\nrustflags = \"-C target-cpu=native\"\n",
        )
        .unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(config.as_std_path()).unwrap();

        assert!(text.contains("loom"), "{text}");
        assert!(text.contains("target-cpu=native"), "{text}");
        // Both keys carry it, because which one cargo picks depends on the target triple.
        assert_eq!(text.matches(CAP_LINTS).count(), 2, "{text}");
    }

    #[test]
    fn a_tree_with_no_cargo_config_gets_one() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(root.join(".cargo").join("config.toml").as_std_path()).unwrap();

        assert!(text.contains(CAP_LINTS), "{text}");
    }

    #[test]
    fn a_cargo_config_with_no_rustflags_gains_them() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let config = root.join(".cargo").join("config.toml");

        fs::create_dir_all(config.parent().unwrap().as_std_path()).unwrap();
        fs::write(config.as_std_path(), "[net]\nretry = 3\n").unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(config.as_std_path()).unwrap();

        assert!(text.contains(CAP_LINTS), "{text}");
        assert!(text.contains("retry"), "{text}");
    }

    #[test]
    fn the_legacy_cargo_config_name_gains_the_cap_too() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let config = root.join(".cargo").join("config");

        fs::create_dir_all(config.parent().unwrap().as_std_path()).unwrap();
        fs::write(config.as_std_path(), "[build]\nrustflags = [\"--cfg\", \"loom\"]\n").unwrap();

        cap_lints(&root).unwrap();

        let text = fs::read_to_string(config.as_std_path()).unwrap();

        assert!(text.contains(CAP_LINTS), "{text}");
        assert!(!root.join(".cargo").join("config.toml").as_std_path().exists());
    }

    #[test]
    fn a_legacy_cargo_config_name_is_anchored() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        fs::create_dir_all(root.join(".cargo").as_std_path()).unwrap();
        fs::write(root.join(".cargo").join("config").as_std_path(), "paths = [\"../vendored\"]\n").unwrap();

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).unwrap();

        let text = fs::read_to_string(root.join(".cargo").join("config").as_std_path()).unwrap();

        // Cargo still accepts `.cargo/config`; it needs the same repair as the TOML-suffixed name.
        assert!(text.contains("/src/vendored"), "{text}");
    }

    #[test]
    fn normalization_keeps_leading_parent_components() {
        // Anchoring from a filesystem root keeps the parent component textually rather than
        // resolving it through the host filesystem.
        assert_eq!(normalize(Utf8Path::new("/../shared")), Utf8PathBuf::from("/../shared"));
    }

    #[test]
    fn a_missing_cargo_config_is_not_an_error() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();

        anchor_cargo_config(&root, Utf8Path::new("/src/app")).unwrap();
    }
}
