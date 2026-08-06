//! Copying a source tree into the scratch directory.
//!
//! Nothing here checks for interruption, deliberately. This tool installs no signal handler, so a
//! Ctrl-C during a long copy takes effect immediately by the default disposition. The scratch tree
//! is then left behind, which costs nothing: the next run clears it before copying, and the lock
//! guarding it is released by the operating system when the process dies. Installing a handler in
//! order to poll for it here would make interruption strictly slower to take effect.

use camino::Utf8Path;
use core::sync::atomic::{AtomicBool, Ordering};
use ignore::{WalkBuilder, WalkState};
use std::fs::{self, File, FileTimes};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::Result;
use crate::error::{Error, error};

/// Version control directories, which are large, hold nothing a build reads, and are actively
/// hazardous in a tree a tool is rewriting: a stray command run from the scratch copy could commit
/// instrumented source over the user's work.
const VCS_DIRS: [&str; 7] = [".git", ".hg", ".bzr", ".svn", "_darcs", ".jj", ".pijul"];

/// Whether copy-on-write cloning is still worth attempting.
///
/// A filesystem either supports reflinks or does not, so one failure settles it for the whole
/// process and the rest of the copy goes straight to a byte-for-byte read. The check is a latch
/// rather than a per-file probe because the failure is not always reported as
/// [`std::io::ErrorKind::Unsupported`] — some platforms return a plain permission or argument
/// error — so there is nothing reliable to match on.
static REFLINK_WORKS: AtomicBool = AtomicBool::new(true);

/// Copies a source tree, skipping build output, version control and the scratch directory itself.
///
/// `skip` is the directory the copy is being written under when that sits inside the source tree,
/// which is the default arrangement; without it the copy would try to copy itself.
///
/// Files ignored by version control are not copied. A tree's own `.gitignore` describes exactly the
/// files that are regenerable or machine-local, and skipping them is usually the difference between
/// copying a source tree and copying a source tree plus everything ever built in it.
pub(super) fn copy_tree(from: &Utf8Path, to: &Utf8Path, skip: &Utf8Path) -> Result<()> {
    fs::create_dir_all(to.as_std_path())
        .map_err(|cause| error!("could not create the scratch tree at `{to}`").caused_by(cause))?;

    let failure: Mutex<Option<Error>> = Mutex::new(None);

    let mut builder = WalkBuilder::new(from.as_std_path());

    let _builder = builder
        // A build reads `.cargo/config.toml`, `.rustfmt.toml` and friends, none of which are
        // hidden in any sense that matters here.
        .hidden(false)
        // Only ignore files inside the tree have any say. Reading them from parent directories
        // means a checkout nested under a directory whose `.gitignore` says `*` copies as nothing
        // at all, and the resulting empty tree fails the build for reasons nobody can see.
        .parents(false)
        // `.gitignore` describes what git would restore, which is only meaningful in something git
        // is actually tracking. Outside a repository the same file is a leftover.
        .require_git(true)
        .git_ignore(true)
        .git_exclude(true)
        // A user's global ignore file describes their machine, not this project, and a rule in it
        // would silently change what a shared tree copies to.
        .git_global(false)
        // `.ignore` is a search convention. It routinely excludes vendored or generated code that
        // a build genuinely needs.
        .ignore(false)
        // A link is recreated rather than followed, so there is nothing to descend into and no
        // cycle to guard against.
        .follow_links(false);

    let root = from.to_owned();
    let destination = to.to_owned();
    let excluded = skip.to_owned();

    builder.build_parallel().run(|| {
        let root = root.clone();
        let destination = destination.clone();
        let excluded = excluded.clone();
        let failure = &failure;

        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,

                // An unreadable directory or a broken entry is reported rather than skipped. A
                // file missing from the copy produces a build failure naming something unrelated,
                // which is far harder to act on than the permission error that caused it.
                Err(cause) => {
                    record(failure, error!("could not read the source tree").caused_by(cause));

                    return WalkState::Quit;
                }
            };

            let Some(source) = Utf8Path::from_path(entry.path()) else {
                record(
                    failure,
                    error!("`{}` is not valid UTF-8 and cannot be copied", entry.path().display()),
                );

                return WalkState::Quit;
            };

            // The walker yields the root itself first, which is the destination, not something to
            // put inside it.
            let Ok(relative) = source.strip_prefix(&root) else {
                return WalkState::Continue;
            };

            if relative.as_str().is_empty() {
                return WalkState::Continue;
            }

            if is_pruned(source, relative, &excluded) {
                return WalkState::Skip;
            }

            match copy_entry(source, &destination.join(relative)) {
                Ok(()) => WalkState::Continue,
                Err(cause) => {
                    record(failure, cause);

                    WalkState::Quit
                }
            }
        })
    });

    match failure.into_inner() {
        Ok(Some(cause)) => Err(cause),
        Ok(None) => Ok(()),

        // The lock is only ever held while recording a failure, so it can only be poisoned by a
        // panic in this crate, and the panic itself is the thing worth reporting.
        Err(poisoned) => poisoned.into_inner().map_or_else(|| Ok(()), Err),
    }
}

/// Records the first failure, which is the one reported.
///
/// Later failures are usually consequences of the first — a walk that hit an unreadable directory
/// tends to hit its siblings too — and the walk is stopping regardless.
fn record(failure: &Mutex<Option<Error>>, cause: Error) {
    if let Ok(mut held) = failure.lock()
        && held.is_none()
    {
        *held = Some(cause);
    }
}

/// Returns whether an entry and everything under it should be left out of the copy.
fn is_pruned(source: &Utf8Path, relative: &Utf8Path, excluded: &Utf8Path) -> bool {
    if source == excluded {
        return true;
    }

    let Some(name) = relative.file_name() else {
        return false;
    };

    if VCS_DIRS.contains(&name) {
        return true;
    }

    // Build output is expensive to copy and regenerated anyway. At the top of the tree the name
    // settles it; deeper down it does not, since `src/target/` is an ordinary module directory, so
    // a nested one has to prove itself by carrying the tag cargo writes into every target
    // directory it owns.
    name == "target" && (relative.parent() == Some(Utf8Path::new("")) || source.join("CACHEDIR.TAG").as_std_path().exists())
}

/// Copies one entry, preserving what it is.
fn copy_entry(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source.as_std_path())
        .map_err(|cause| error!("could not read `{source}`").caused_by(cause))?;

    if metadata.is_dir() {
        return fs::create_dir_all(destination.as_std_path())
            .map_err(|cause| error!("could not create `{destination}`").caused_by(cause));
    }

    // Every entry creates its own parent rather than relying on having seen the directory first.
    // The walk is parallel, so the thread holding a file and the thread holding its directory are
    // not ordered against each other.
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent.as_std_path())
            .map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
    }

    if metadata.is_symlink() {
        return copy_symlink(source, destination);
    }

    copy_file(source, destination)
}

/// Recreates a symlink rather than copying what it points at.
///
/// Following the link instead would materialize its target inside the scratch tree, which for a
/// link pointing outside the workspace — a home directory, a data mount — means copying that
/// wholesale. The link is reproduced verbatim, including a relative or broken one, since a build
/// that worked with it in the original tree is the thing being reproduced.
fn copy_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let target = fs::read_link(source.as_std_path())
        .map_err(|cause| error!("could not read the link `{source}`").caused_by(cause))?;

    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(&target, destination.as_std_path());

    #[cfg(windows)]
    let created = if target.is_dir() {
        std::os::windows::fs::symlink_dir(&target, destination.as_std_path())
    } else {
        std::os::windows::fs::symlink_file(&target, destination.as_std_path())
    };

    created.map_err(|cause| error!("could not recreate the link `{destination}`").caused_by(cause))
}

/// Copies one file, cloning it if the filesystem can.
fn copy_file(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    if reflink_supported() && REFLINK_WORKS.load(Ordering::Relaxed) {
        match reflink_copy::reflink(source.as_std_path(), destination.as_std_path()) {
            Ok(()) => {
                freshen(destination);

                return Ok(());
            }
            Err(_unsupported) => {
                REFLINK_WORKS.store(false, Ordering::Relaxed);

                // A failed clone can still leave a file behind, and the copy below will not
                // overwrite what it did not create.
                let _removed = fs::remove_file(destination.as_std_path());
            }
        }
    }

    let _bytes = fs::copy(source.as_std_path(), destination.as_std_path())
        .map_err(|cause| error!("could not copy `{source}` to `{destination}`").caused_by(cause))?;

    Ok(())
}

/// Whether cloning is worth trying on this platform at all.
///
/// On musl the copy syscall the crate reaches for is not the standard one, and asking for a clone
/// there fails in ways that are not worth distinguishing from a filesystem that cannot do it.
const fn reflink_supported() -> bool {
    !cfg!(target_env = "musl")
}

/// Stamps a cloned file with the current time.
///
/// A clone preserves the source's modification time on some platforms, which leaves a fresh copy
/// looking arbitrarily old. On macOS that is not merely cosmetic: the system prunes files under
/// `/var/folders` once they pass three days, so a scratch tree cloned from an old checkout can
/// have files deleted out from under a run that is still using them.
fn freshen(destination: &Utf8Path) {
    if let Ok(file) = File::options().write(true).open(destination.as_std_path()) {
        let _stamped = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;

    fn tree() -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let from = Utf8PathBuf::from_path_buf(temporary.path().join("from")).unwrap();
        let to = Utf8PathBuf::from_path_buf(temporary.path().join("to")).unwrap();

        fs::create_dir_all(from.as_std_path()).unwrap();

        (temporary, from, to)
    }

    #[test]
    fn build_output_and_version_control_are_skipped() {
        let (_temporary, from, to) = tree();

        for directory in ["src", "target", ".git", ".jj", "_darcs"] {
            fs::create_dir_all(from.join(directory).as_std_path()).unwrap();
            fs::write(from.join(directory).join("f").as_std_path(), "x").unwrap();
        }

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("src").join("f").as_std_path().exists());

        for skipped in ["target", ".git", ".jj", "_darcs"] {
            assert!(!to.join(skipped).as_std_path().exists(), "{skipped} was copied");
        }
    }

    #[test]
    fn a_nested_target_module_survives() {
        // `src/target/` is an ordinary module name. Only a directory carrying cargo's tag is
        // build output.
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("src").join("target").as_std_path()).unwrap();
        fs::write(from.join("src").join("target").join("mod.rs").as_std_path(), "fn f() {}").unwrap();

        fs::create_dir_all(from.join("nested").join("target").as_std_path()).unwrap();
        fs::write(from.join("nested").join("target").join("CACHEDIR.TAG").as_std_path(), "").unwrap();
        fs::write(from.join("nested").join("target").join("junk").as_std_path(), "x").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("src").join("target").join("mod.rs").as_std_path().exists());
        assert!(!to.join("nested").join("target").join("junk").as_std_path().exists());
    }

    #[test]
    fn nested_directories_are_recreated() {
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("a").join("b").join("c").as_std_path()).unwrap();
        fs::write(from.join("a").join("b").join("c").join("deep.rs").as_std_path(), "fn f() {}").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert_eq!(
            fs::read_to_string(to.join("a").join("b").join("c").join("deep.rs").as_std_path()).unwrap(),
            "fn f() {}"
        );
    }

    #[test]
    fn the_scratch_directory_is_not_copied_into_itself() {
        let (_temporary, from, to) = tree();
        let skip = from.join("scratch");

        fs::create_dir_all(skip.as_std_path()).unwrap();
        fs::write(skip.join("f").as_std_path(), "x").unwrap();
        fs::create_dir_all(from.join("src").as_std_path()).unwrap();

        copy_tree(&from, &to, &skip).unwrap();

        assert!(!to.join("scratch").as_std_path().exists());
        assert!(to.join("src").as_std_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_recreated_rather_than_followed() {
        // Following it would materialize whatever it points at, which for a link out of the
        // workspace means copying an arbitrary part of the filesystem.
        let (_temporary, from, to) = tree();
        let outside = from.parent().unwrap().join("outside");

        fs::create_dir_all(outside.as_std_path()).unwrap();
        fs::write(outside.join("secret").as_std_path(), "x").unwrap();

        std::os::unix::fs::symlink(outside.as_std_path(), from.join("link").as_std_path()).unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        let copied = to.join("link");

        assert!(fs::symlink_metadata(copied.as_std_path()).unwrap().is_symlink());
        assert_eq!(fs::read_link(copied.as_std_path()).unwrap(), outside.as_std_path());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_does_not_hang_the_copy() {
        // The old hand-rolled walk followed links and needed a depth cap to survive this.
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("a").as_std_path()).unwrap();
        std::os::unix::fs::symlink(from.as_std_path(), from.join("a").join("loop").as_std_path()).unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(fs::symlink_metadata(to.join("a").join("loop").as_std_path()).unwrap().is_symlink());
    }

    #[test]
    fn a_deep_tree_is_copied_whole() {
        // The old walk stopped at 64 levels and returned success, losing everything below.
        let (_temporary, from, to) = tree();
        let mut deep = from.clone();

        for _level in 0..80 {
            deep = deep.join("d");
        }

        fs::create_dir_all(deep.as_std_path()).unwrap();
        fs::write(deep.join("bottom.rs").as_std_path(), "fn f() {}").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        let landed = to.join(deep.strip_prefix(&from).unwrap());

        assert!(landed.join("bottom.rs").as_std_path().exists());
    }

    #[test]
    fn an_empty_directory_is_preserved() {
        // A build script can expect a directory to exist without anything being in it.
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("empty").as_std_path()).unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("empty").as_std_path().is_dir());
    }

    #[test]
    fn a_missing_source_tree_is_reported() {
        let (_temporary, from, to) = tree();
        let missing = from.join("absent");

        let cause = copy_tree(&missing, &to, Utf8Path::new("/nowhere")).unwrap_err();

        assert!(cause.to_string().contains("could not read the source tree"), "{cause}");
    }

    #[test]
    fn a_destination_entry_that_cannot_be_replaced_is_reported() {
        let (_temporary, from, to) = tree();

        fs::write(from.join("file").as_std_path(), "source").unwrap();
        fs::create_dir_all(to.join("file").as_std_path()).unwrap();

        let cause = copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap_err();

        // The parallel walk records the first failed copy rather than continuing and reporting a
        // later build error about whatever was missing from the scratch tree.
        assert!(cause.to_string().contains("could not copy"), "{cause}");
    }

    #[test]
    fn pruning_an_entry_without_a_file_name_is_not_a_match() {
        // The root entry has no real file name after it is stripped, and must not be mistaken for
        // a version-control or target directory.
        assert!(!is_pruned(
            Utf8Path::new("/workspace"),
            Utf8Path::new(""),
            Utf8Path::new("/elsewhere"),
        ));
    }

    #[test]
    fn freshening_a_missing_file_is_harmless() {
        let (_temporary, _from, to) = tree();
        let file = to.join("copied");

        fs::create_dir_all(to.as_std_path()).unwrap();
        fs::write(file.as_std_path(), "x").unwrap();
        freshen(&file);
        fs::remove_file(file.as_std_path()).unwrap();
        freshen(&file);

        // Freshening is best-effort metadata repair after a reflink; it must never turn a
        // successful copy into an error just because timestamps cannot be changed.
        assert!(!file.as_std_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_source_entry_is_reported() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let (_temporary, from, to) = tree();
        let name = OsString::from_vec(b"bad-\xff".to_vec());

        fs::write(from.as_std_path().join(name), "x").unwrap();

        let cause = copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap_err();

        // Paths in reports and manifests are UTF-8; a lossy copy would make the later error point
        // at a name the user cannot match back to the source tree.
        assert!(cause.to_string().contains("not valid UTF-8"), "{cause}");
    }
}
