//! Everything a run needs, worked out before any building happens.

use camino::{Utf8Path, Utf8PathBuf};

use super::target_file::TargetFile;
use crate::model::Mutant;

/// Everything a run needs, worked out before any building happens.
#[derive(Debug)]
pub struct Plan {
    /// Absolute path of the workspace root.
    pub root: Utf8PathBuf,

    /// The files that were analyzed.
    pub files: Vec<TargetFile>,

    /// Every mutant to be tested, with ordinals assigned.
    pub mutants: Vec<Mutant>,

    /// Mutants suppressed by a directive. They stay in `mutants`, marked as ignored.
    pub suppressed: usize,

    /// Mutants excluded by sharding, counted rather than kept.
    pub sharded_out: usize,

    /// Mutants an earlier report already settled, dropped rather than run again.
    pub settled_out: usize,

    /// For each workspace package, the workspace packages its test binaries can reach.
    ///
    /// A test binary can only exercise code it links, so a mutant in a package outside this set is
    /// unreachable from that binary no matter what the tests do. Running it anyway is pure cost.
    pub reach: crate::HashMap<String, crate::HashSet<String>>,

    /// For each workspace package, its manifest directory relative to the root, and its version.
    ///
    /// See [`Plan::spec`] for why a name on its own will not do.
    pub specs: crate::HashMap<String, (Utf8PathBuf, String)>,
}

impl Plan {
    /// Names one package unambiguously, for a `--package` argument in a tree rooted at `root`.
    ///
    /// `--package serde` is ambiguous the moment a workspace member shares its name with a crate
    /// anywhere in the dependency graph, and a repository that dev-depends on a published version
    /// of itself does exactly that. Cargo rejects the whole invocation, and it rejects it before
    /// emitting any JSON, so a build that fails this way arrives with nothing to attribute and is
    /// indistinguishable from a tree that does not compile. Spelling the package as a path-rooted
    /// package ID makes the question unambiguous by construction, so it is asked that way always
    /// rather than only once cargo has complained.
    ///
    /// Falls back to the bare name for a package whose manifest was never located, which is no
    /// worse than what came before.
    #[must_use]
    pub fn spec(&self, root: &Utf8Path, package: &str) -> String {
        let Some((directory, version)) = self.specs.get(package) else {
            return package.to_owned();
        };

        let absolute = if directory.as_str().is_empty() {
            root.to_owned()
        } else {
            root.join(directory)
        };

        let normalized = absolute.as_str().replace('\\', "/");
        let root_slash = if normalized.starts_with('/') { "" } else { "/" };
        let source = format!("path+file://{root_slash}{normalized}");

        format!("{source}#{package}@{version}")
    }
    /// Folds one package's scan into the plan.
    ///
    /// A run scans a package, instruments it and builds it before moving to the next, so the plan
    /// is assembled a package at a time rather than handed over complete.
    pub fn absorb(&mut self, scanned: super::Scanned) {
        let super::Scanned { mutants, suppressed, sharded_out, settled_out } = scanned;

        self.mutants.extend(mutants);
        self.suppressed = self.suppressed.saturating_add(suppressed);
        self.sharded_out = self.sharded_out.saturating_add(sharded_out);
        self.settled_out = self.settled_out.saturating_add(settled_out);
    }

    /// Puts the mutants in report order, once every package has been absorbed.
    pub fn sort(&mut self) {
        self.mutants.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.span.start.cmp(&right.span.start))
                .then_with(|| left.mutator.cmp(&right.mutator))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(specs: &[(&str, &str, &str)]) -> Plan {
        Plan {
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: specs
                .iter()
                .map(|(name, dir, version)| {
                    ((*name).to_owned(), (Utf8PathBuf::from(*dir), (*version).to_owned()))
                })
                .collect(),
        }
    }

    #[test]
    fn a_package_is_named_by_where_its_manifest_is() {
        // Cargo rejects an ambiguous `--package` before it emits any JSON, so the build fails with
        // nothing to attribute and reads as though the tree does not compile. A path-rooted id
        // cannot be ambiguous.
        let plan = plan(&[("serde", "serde", "1.0.0")]);

        assert_eq!(
            plan.spec(Utf8Path::new("/tree"), "serde"),
            "path+file:///tree/serde#serde@1.0.0"
        );
    }

    #[test]
    fn a_single_crate_repository_names_its_root() {
        // The manifest sits at the workspace root, so the relative directory is empty and joining
        // it blindly would produce the trailing separator that cargo does not match.
        let plan = plan(&[("itoa", "", "1.0.18")]);

        assert_eq!(plan.spec(Utf8Path::new("/tree"), "itoa"), "path+file:///tree#itoa@1.0.18");
    }

    #[test]
    fn the_spec_follows_the_tree_it_is_asked_about() {
        // A run builds in a scratch copy, not in the user's checkout, and a spec naming the wrong
        // root would select the wrong package or none at all.
        let plan = plan(&[("core", "crates/core", "0.2.0")]);

        assert_eq!(
            plan.spec(Utf8Path::new("/scratch/tree"), "core"),
            "path+file:///scratch/tree/crates/core#core@0.2.0"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_package_spec_is_a_file_url() {
        // Cargo package IDs are URLs even on Windows. Passing a native `D:\...` path makes Cargo
        // parse the drive letter as URL syntax and silently changes the package being named.
        let plan = plan(&[("core", "crates/core", "0.2.0")]);

        assert_eq!(
            plan.spec(Utf8Path::new(r"D:\scratch\tree"), "core"),
            "path+file:///D:/scratch/tree/crates/core#core@0.2.0"
        );
    }

    #[test]
    fn a_package_with_no_manifest_recorded_keeps_its_bare_name() {
        // Erring toward the old behaviour: a bare name builds the right thing whenever it is not
        // ambiguous, whereas a malformed id fails every time.
        let plan = plan(&[]);

        assert_eq!(plan.spec(Utf8Path::new("/tree"), "lonely"), "lonely");
    }
}
