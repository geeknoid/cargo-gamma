use camino::{Utf8Path, Utf8PathBuf};
use core::sync::atomic::{AtomicBool, Ordering};
use std::env;
use std::fs::{self, File};
use std::process::Command;
use walkdir::WalkDir;

use crate::Result;
use crate::discover::TargetFile;
use crate::error::error;

use super::cargo_options::CargoOptions;
use super::config::Config;
use super::copy::copy_tree;
use super::events::Events;
use super::loader::toolchain_libraries;
use super::manifest::{CAP_LINTS, Manifest, RUNTIME_CRATE, anchor_cargo_config, cap_lints};

/// The guard runtime's source, embedded so that the vendored copy cannot drift from the real one.
const RUNTIME_SOURCE: &str = include_str!("../../../cargo-gamma-rt/src/lib.rs");

/// A scratch copy of the workspace, instrumented and ready to build.
#[derive(Debug)]
pub struct Workspace {
    /// Root of the copied tree.
    pub(super) root: Utf8PathBuf,

    /// Where build artifacts go, kept outside the copied tree so that repeated runs are
    /// incremental rather than starting cold every time.
    pub(super) target: Utf8PathBuf,

    /// Directories a test binary needs on its dynamic loader path.
    ///
    /// `cargo test` sets this before running a binary it built; we run binaries ourselves, so we
    /// have to reproduce it for toolchains that link `std` dynamically.
    pub(super) libraries: Vec<Utf8PathBuf>,

    /// How cargo is invoked in this tree.
    pub(super) cargo: CargoOptions,

    /// Where the vendored guard runtime lives, so that a package can be linked to it at the moment
    /// its own mutants are known rather than all of them up front.
    runtime: Utf8PathBuf,

    /// Whether the tree survives the run for inspection.
    pub(super) leak: bool,

    /// Whether the run got far enough to be worth keeping build artifacts for.
    ///
    /// Artifacts are what make the next run incremental, so a run that reached the point of having
    /// something to measure leaves them behind on purpose. A run that failed before then leaves
    /// nothing a later run could reuse — only the object files of a tree that no longer exists,
    /// which on a large workspace is tens of gigabytes of dead weight on a disk that a CI job may
    /// well need for its next step.
    settled: AtomicBool,

    /// Held for the life of the run so that a second run in the same scratch directory is turned
    /// away rather than allowed to delete this one's tree out from under it. Released when the
    /// process ends, however it ends, so a crash cannot leave a lock nobody can clear.
    _lock: File,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.leak {
            return;
        }

        // The tree is rebuilt from scratch on the next run, so failing to remove it costs disk
        // rather than correctness, and there is no caller in a position to do anything about it.
        let _removed = fs::remove_dir_all(self.root.as_std_path());

        if !self.settled.load(Ordering::Relaxed) {
            let _removed = fs::remove_dir_all(self.target.as_std_path());
        }
    }
}

impl Workspace {
    /// Says where to look at the tree, for an error that wants the reader to go and read it.
    ///
    /// The tree is deleted when the run ends unless `--leak-dirs` was given, so naming its path
    /// unconditionally sends the reader to a directory that no longer exists by the time they get
    /// there — which reads as a second, phantom bug on top of the one being reported.
    pub(super) fn inspect_hint(&self) -> String {
        if self.leak {
            format!("The tree is at `{}` if you want to look.", self.root)
        } else {
            "Re-run with `--leak-dirs` to keep the instrumented tree and look at it.".to_owned()
        }
    }

    /// Copies and instruments the tree.
    pub(super) fn prepare(source: &Utf8Path, config: &Config, events: &mut impl Events) -> Result<Self> {
        let base = gamma_base(source, config.scratch_dir.as_deref());
        let root = base.join("tree");
        let target = base.join("build");
        let runtime = base.join("rt");

        events.phase("Copying", "the workspace");

        fs::create_dir_all(base.as_std_path())
            .map_err(|cause| error!("could not create the scratch directory at `{base}`").caused_by(cause))?;

        let lock = claim(&base)?;

        if root.as_std_path().exists() {
            fs::remove_dir_all(root.as_std_path())
                .map_err(|cause| error!("could not clear the scratch tree at `{root}`").caused_by(cause))?;
        }

        copy_tree(source, &root, &base)?;
        vendor_runtime(&runtime)?;
        anchor_manifests(source, &root)?;

        let libraries = toolchain_libraries(&root, &target);

        Ok(Self {
            root,
            target,
            libraries,
            cargo: config.cargo.clone(),
            runtime,
            leak: config.leak_dirs,
            settled: AtomicBool::new(false),
            _lock: lock,
        })
    }

    /// Adds the guard runtime as a dependency of one package.
    ///
    /// Called when a package is about to be instrumented, not when the tree is copied: which
    /// packages need it is not known until they have been scanned, and a package that turned out
    /// to have no mutants should not carry a dependency it never uses.
    pub(super) fn link_runtime(&self, package: &str, files: &[TargetFile]) -> Result<()> {
        let runtime = &self.runtime;
        let Some(path) = self.manifest_of(package, files) else {
            return Ok(());
        };

        let mut manifest = Manifest::read(&path)?;

        manifest.link_runtime(runtime);
        manifest.save()
    }

    /// Locates a package's manifest inside the copied tree.
    fn manifest_of(&self, package: &str, files: &[TargetFile]) -> Option<Utf8PathBuf> {
        let file = files.iter().find(|file| file.package == package)?;
        let mut directory = self.root.join(&file.path);

        // Walk up from a source file until a manifest appears, which is the package root.
        while directory.pop() {
            let candidate = directory.join("Cargo.toml");

            if candidate.as_std_path().is_file() {
                return Some(candidate);
            }

            if directory == self.root {
                break;
            }
        }

        None
    }

    /// Replaces a file that the copy already put in the tree.
    ///
    /// Refuses to follow a symlink or to create a file that was not copied, because either means
    /// writing somewhere the copy did not choose — through a link, that is somewhere outside the
    /// scratch tree entirely, and the tree holds a copy of the user's real source.
    /// Returns whether the file had to be written, so a caller can report what it really changed.
    pub(super) fn overwrite(path: &Utf8Path, contents: &str) -> Result<bool> {
        let metadata = fs::symlink_metadata(path.as_std_path())
            .map_err(|cause| error!("could not write `{path}`, which the copy did not create").caused_by(cause))?;

        if !metadata.is_file() {
            return Err(error!(
                "refusing to write `{path}`, which is a link or a device rather than the copied source file"
            ));
        }

        // Cargo decides what to recompile from mtime, not from content, so writing a file back
        // byte-for-byte still rebuilds its crate and everything downstream of it. The rollback loop
        // rewrites the whole tree every round while changing only the few files whose mutants were
        // withdrawn, which made every round cost a full workspace build. Comparing first turns the
        // untouched majority into a read.
        if let Ok(existing) = fs::read(path.as_std_path())
            && existing == contents.as_bytes()
        {
            return Ok(false);
        }

        fs::write(path.as_std_path(), contents)
            .map_err(|cause| error!("could not write `{path}`").caused_by(cause))?;

        Ok(true)
    }

    /// Runs a cargo command in the copied tree.
    /// Wraps an existing directory as a workspace, without copying or locking anything.
    ///
    /// Only for tests that need a real tree to run cargo in. The tree is left in place on drop,
    /// because it belongs to whatever created it rather than to this handle.
    #[cfg(test)]
    pub(crate) fn adopt(root: Utf8PathBuf, target: Utf8PathBuf) -> Self {
        Self {
            runtime: root.join("gamma-rt"),
            root,
            target,
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            settled: AtomicBool::new(true),
            leak: true,
            _lock: tempfile::tempfile().expect("a temporary file should be creatable"),
        }
    }

    /// Replaces the arguments every test binary is launched with.
    ///
    /// Only for tests that stand a shell in for a compiled harness, where the script to run is
    /// itself an argument.
    #[cfg(test)]
    pub(crate) fn set_test_args(&mut self, args: Vec<String>) {
        self.cargo.test_args = args;
    }

    pub(super) fn cargo(&self) -> Command {
        let mut command = Command::new(cargo_binary());

        let _ = command.current_dir(self.root.as_std_path());
        let _ = command.env("CARGO_TARGET_DIR", self.target.as_std_path());

        // The instrumented tree is not the user's code, and lint levels configured for their code
        // have no authority over ours; denying warnings here would fail on any guarded expression a
        // lint happens to dislike. The flag is normally merged into the copied tree's
        // `.cargo/config.toml` by `cap_lints`, because setting `RUSTFLAGS` would *replace* the
        // tree's configured flags rather than add to them. An ambient setting outranks the
        // configuration, though, so when the caller has one it has to be extended here instead.
        //
        // `CARGO_ENCODED_RUSTFLAGS` wins over `RUSTFLAGS`, and its separator is a unit separator
        // rather than a space, so only one of the two is ever extended: whichever cargo will read.
        if let Some(inherited) = env::var_os("CARGO_ENCODED_RUSTFLAGS") {
            let mut merged = inherited;

            merged.push("\u{1f}");
            merged.push(CAP_LINTS);

            let _ = command.env("CARGO_ENCODED_RUSTFLAGS", merged);
        } else if let Some(inherited) = env::var_os("RUSTFLAGS") {
            let mut merged = inherited;

            merged.push(" ");
            merged.push(CAP_LINTS);

            let _ = command.env("RUSTFLAGS", merged);
        }

        // A guard is inert unless this names its mutant. Proc macros run inside the compiler, so a
        // live mutant in one executes during the build rather than during a test — an infinite loop
        // there hangs the one build the whole run depends on, with no test to time it out. The
        // variable is scrubbed rather than trusted to be absent, since test processes set it and a
        // user debugging a mutant by hand may export it.
        let _ = command.env_remove(gamma_rt::ACTIVE_VAR);

        command
    }

    /// Marks the run as having got far enough that its build artifacts are worth keeping.
    ///
    /// Until this is called, dropping the tree also discards everything built for it: the artifacts
    /// of a run that never produced a result are of no use to the next one, and on a large
    /// workspace they can be tens of gigabytes.
    pub(super) fn settle(&self) {
        self.settled.store(true, Ordering::Relaxed);
    }

    /// Total size in bytes of everything this run keeps on disk.
    ///
    /// Walking the tree costs a stat per file, which is cheap next to any run that has artifacts
    /// worth measuring, and unreadable entries are skipped rather than failing a run that has
    /// already succeeded.
    pub(super) fn footprint(&self) -> u64 {
        let base = self.root.parent().unwrap_or(&self.root);

        WalkDir::new(base.as_std_path())
            .into_iter()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.metadata().ok())
            .filter(fs::Metadata::is_file)
            .map(|metadata| metadata.len())
            .sum()
    }
}

/// Where this tool keeps everything it generates for a workspace.
///
/// Under the workspace's own `target` by default, so that a checkout carries its scratch tree with
/// it and a stale one is cleaned by `cargo clean`. The default matters for speed as well as
/// tidiness: build artifacts live beside the tree and survive between runs, and a path that moved
/// would make every run compile from cold.
#[must_use]
pub fn gamma_base(root: &Utf8Path, scratch: Option<&Utf8Path>) -> Utf8PathBuf {
    scratch.map_or_else(|| root.join("target").join("gamma"), |directory| directory.join("gamma"))
}

/// Where the instrumented copy of a workspace lives.
///
/// Derived rather than stored so that a caller which was asked to keep the tree can say where it
/// is without the run having to hand back the workspace that owned it.
#[must_use]
pub fn scratch_tree(root: &Utf8Path, scratch: Option<&Utf8Path>) -> Utf8PathBuf {
    gamma_base(root, scratch).join("tree")
}

/// Takes the scratch directory for the duration of the run.
///
/// Two runs sharing one scratch directory would each delete the other's tree and write build
/// artifacts into a single directory under two different sets of instrumented sources, producing
/// verdicts belonging to neither. The lock is advisory and held by an open file, so it is released
/// when the process ends however it ends — there is no stale lock to clear after a crash.
fn claim(base: &Utf8Path) -> Result<File> {
    let path = base.join("lock");

    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.as_std_path())
        .map_err(|cause| error!("could not open the scratch lock at `{path}`").caused_by(cause))?;

    file.try_lock().map_err(|_held| {
        error!(
            "another `cargo gamma` run is already using `{base}`.\n\
             Wait for it to finish, or give this run a scratch directory of its own with --scratch-dir."
        )
        .usage()
    })?;

    Ok(file)
}

/// Repairs every manifest in the copied tree so that it still resolves from its new location.
///
/// Every manifest is visited, not just the ones belonging to mutated packages: a package nobody
/// mutates is still built, and a path dependency it cannot resolve fails the build just as surely.
fn anchor_manifests(source: &Utf8Path, root: &Utf8Path) -> Result<()> {
    for entry in WalkDir::new(root.as_std_path()).into_iter().filter_map(core::result::Result::ok) {
        if entry.file_name() != "Cargo.toml" {
            continue;
        }

        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };

        let Some(original) = path.parent().and_then(|directory| directory.strip_prefix(root).ok()) else {
            continue;
        };

        let mut manifest = Manifest::read(path)?;

        manifest.anchor_paths(&source.join(original), original);
        manifest.save()?;
    }

    anchor_cargo_config(root, source)?;

    // Done here rather than through `RUSTFLAGS`, which would replace the tree's own flags
    // instead of adding to them.
    cap_lints(root)
}

/// Returns the cargo executable to use, honouring the one that invoked us.
fn cargo_binary() -> String {
    env::var("CARGO").unwrap_or_else(|_missing| "cargo".to_owned())
}

/// Writes the guard runtime as a standalone crate outside the copied workspace.
///
/// Outside deliberately: a path dependency living inside a workspace directory but absent from its
/// member list makes cargo refuse to build, and editing the user's member list would be far more
/// invasive than dropping a crate next door.
fn vendor_runtime(at: &Utf8Path) -> Result<()> {
    let source = at.join("src");

    fs::create_dir_all(source.as_std_path())
        .map_err(|cause| error!("could not create `{source}`").caused_by(cause))?;

    let manifest = format!(
        "[package]\nname = \"{RUNTIME_CRATE}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [lib]\npath = \"src/lib.rs\"\n\n[workspace]\n"
    );

    fs::write(at.join("Cargo.toml").as_std_path(), manifest)
        .map_err(|cause| error!("could not write the runtime manifest in `{at}`").caused_by(cause))?;

    fs::write(source.join("lib.rs").as_std_path(), RUNTIME_SOURCE)
        .map_err(|cause| error!("could not write the runtime source in `{source}`").caused_by(cause))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree that cannot be cleared is reported rather than silently reused.
    #[test]
    fn a_scratch_tree_that_cannot_be_cleared_is_a_reported_failure() {
        let directory = crate::testing::workdir("stale-tree");
        let source = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");

        fs::write(source.join("Cargo.toml").as_std_path(), "[workspace]\nmembers = []\n").expect("a manifest");

        // A leftover named `tree` that is a file rather than a directory cannot be removed as one.
        // Reusing it would instrument nothing and report a perfect score, so the run has to stop.
        let base = gamma_base(&source, None);

        fs::create_dir_all(base.as_std_path()).expect("the scratch base");
        fs::write(base.join("tree").as_std_path(), "not a directory").expect("the stale entry");

        let config = Config::default();
        let mut events = crate::testing::Recorder::default();
        let failure = Workspace::prepare(&source, &config, &mut events).expect_err("clearing the tree must fail");

        assert!(failure.to_string().contains("could not clear the scratch tree"), "{failure}");
    }

    #[test]
    fn the_vendored_runtime_is_the_real_one() {
        // If these ever diverge, guards would be compiled against a runtime that is not the one
        // this build was tested with.
        assert!(RUNTIME_SOURCE.contains("pub fn a(id: u32) -> bool"));
        assert!(RUNTIME_SOURCE.contains("GAMMA_ACTIVE"));
    }

    #[test]
    fn vendoring_writes_a_buildable_crate() {
        let temporary = tempfile::tempdir().unwrap();
        let at = Utf8PathBuf::from_path_buf(temporary.path().join("rt")).unwrap();

        vendor_runtime(&at).unwrap();

        let manifest = fs::read_to_string(at.join("Cargo.toml").as_std_path()).unwrap();

        assert!(manifest.contains("name = \"gamma_rt\""));

        // The `[workspace]` table keeps it from being adopted by whatever workspace it lands near.
        assert!(manifest.contains("[workspace]"));
        assert!(at.join("src").join("lib.rs").as_std_path().is_file());
    }

    #[test]
    fn a_run_that_never_settled_takes_its_build_output_with_it() {
        // Artifacts of a tree that no longer exists cannot make anything incremental, and on a
        // large workspace they are tens of gigabytes on a disk a CI job still has plans for.
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("tree");
        let target = base.join("build");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();
        fs::write(target.join("artifact").as_std_path(), "x").unwrap();

        drop(unsettled(&base, &root, &target));

        assert!(!root.as_std_path().exists());
        assert!(!target.as_std_path().exists());
    }

    #[test]
    fn a_run_that_settled_keeps_its_build_output_for_the_next_one() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("tree");
        let target = base.join("build");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let work = unsettled(&base, &root, &target);

        work.settle();
        drop(work);

        assert!(!root.as_std_path().exists(), "the tree is rewritten every run either way");
        assert!(target.as_std_path().exists());
    }

    #[test]
    fn a_leaked_tree_keeps_everything() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("tree");
        let target = base.join("build");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);

        work.leak = true;
        drop(work);

        assert!(root.as_std_path().exists());
        assert!(target.as_std_path().exists());
    }

    #[test]
    fn the_footprint_counts_everything_the_run_leaves_behind() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("tree");
        let target = base.join("build");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();
        fs::write(root.join("a.rs").as_std_path(), "0123456789").unwrap();
        fs::write(target.join("a.o").as_std_path(), "01234").unwrap();

        let mut work = unsettled(&base, &root, &target);

        assert_eq!(work.footprint(), 15);

        work.leak = true;
    }

    /// A workspace over a real directory that has not been marked as worth keeping.
    fn unsettled(base: &Utf8Path, root: &Utf8Path, target: &Utf8Path) -> Workspace {
        Workspace {
            root: root.to_owned(),
            runtime: base.join("rt"),
            target: target.to_owned(),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            settled: AtomicBool::new(false),
            leak: false,
            _lock: File::create(base.join("lock").as_std_path()).unwrap(),
        }
    }

    #[test]
    fn the_build_cannot_see_an_active_mutant() {
        // A live mutant inside a proc macro would run inside rustc and could hang the one build
        // the whole run depends on.
        let work = Workspace {
            root: Utf8PathBuf::from("/tmp/gamma-root"),
            runtime: Utf8PathBuf::from("/tmp/gamma-rt"),
            target: Utf8PathBuf::from("/tmp/gamma-target"),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            settled: AtomicBool::new(true),
            // This one was never created, so nothing should try to remove it when it drops.
            leak: true,
            _lock: tempfile::tempfile().unwrap(),
        };

        let command = work.cargo();
        let scrubbed = command
            .get_envs()
            .any(|(key, value)| key == gamma_rt::ACTIVE_VAR && value.is_none());

        assert!(scrubbed, "the build environment must not carry {}", gamma_rt::ACTIVE_VAR);
    }

    #[test]
    fn scratch_tree_is_derived_from_the_same_base_as_prepare() {
        // Callers report this path after deliberately leaking the workspace, so it has to match
        // the tree `prepare` would have created.
        assert_eq!(
            scratch_tree(Utf8Path::new("/workspace"), None),
            Utf8PathBuf::from("/workspace/target/gamma/tree")
        );
        assert_eq!(
            scratch_tree(Utf8Path::new("/workspace"), Some(Utf8Path::new("/scratch"))),
            Utf8PathBuf::from("/scratch/gamma/tree")
        );
    }

    #[test]
    fn linking_a_package_with_no_manifest_is_a_noop() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        let work = Workspace {
            root,
            runtime: Utf8PathBuf::from("/scratch/rt"),
            target: Utf8PathBuf::from("/scratch/build"),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            settled: AtomicBool::new(true),
            leak: true,
            _lock: tempfile::tempfile().unwrap(),
        };
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: Utf8PathBuf::from("/source/crate/src/lib.rs"),
        }];

        work.link_runtime("pkg", &files).unwrap();

        // A scanned file might be malformed or synthetic in tests; absent manifests are ignored
        // rather than making runtime linking fail before instrumentation can explain anything.
        assert!(!work.root.join("Cargo.toml").as_std_path().exists());
    }

    #[test]
    fn manifest_lookup_stops_at_the_workspace_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        let work = Workspace {
            root,
            runtime: Utf8PathBuf::from("/scratch/rt"),
            target: Utf8PathBuf::from("/scratch/build"),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            settled: AtomicBool::new(true),
            leak: true,
            _lock: tempfile::tempfile().unwrap(),
        };
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: Utf8PathBuf::from("/source/crate/src/lib.rs"),
        }];

        // Walking above the copied root would let an unrelated parent manifest claim this package.
        assert_eq!(work.manifest_of("pkg", &files), None);
    }

    #[test]
    fn overwriting_a_directory_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("not-a-file")).unwrap();

        fs::create_dir_all(path.as_std_path()).unwrap();

        let cause = Workspace::overwrite(&path, "new").unwrap_err();

        // Instrumentation only writes files the copy already chose; refusing other entry kinds
        // prevents following a link or clobbering a device outside the scratch tree.
        assert!(cause.to_string().contains("refusing to write"), "{cause}");
    }

    #[test]
    fn a_second_claim_on_the_same_scratch_directory_is_a_usage_error() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().join("gamma")).unwrap();

        fs::create_dir_all(base.as_std_path()).unwrap();
        let _held = claim(&base).unwrap();
        let cause = claim(&base).unwrap_err();

        // The lock protects concurrent runs from deleting each other's copied trees.
        assert!(cause.is_usage());
        assert!(cause.to_string().contains("already using"), "{cause}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_manifests_are_skipped_while_anchoring() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let source = Utf8PathBuf::from_path_buf(temporary.path().join("source")).unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let name = OsString::from_vec(b"bad-\xff".to_vec());
        let bad = root.as_std_path().join(name);

        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("Cargo.toml"), "[package]\nname = \"bad\"\nversion = \"0.0.0\"\n").unwrap();

        anchor_manifests(&source, &root).unwrap();

        // A path that cannot appear in cargo's UTF-8 JSON is left alone rather than poisoning the
        // whole copied tree repair pass.
        assert!(bad.join("Cargo.toml").exists());
    }
}
