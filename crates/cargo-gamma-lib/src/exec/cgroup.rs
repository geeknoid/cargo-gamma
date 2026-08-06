//! Metering and bounding a test subtree's memory with a cgroup v2 leaf.
//!
//! cgroup v2 is the only unprivileged Linux facility that accounts for a whole process tree as one
//! quantity, which is exactly what a test binary is: a harness plus whatever servers, databases and
//! nested builds it starts. Every invocation gets a freshly created leaf cgroup, so the accounting
//! starts at zero without relying on a kernel that supports resetting `memory.peak`, and so one
//! mutant's peak can never be attributed to the next.
//!
//! The child is moved into that leaf by the child itself, from a [`CommandExt::pre_exec`] hook, and
//! not by this process after the spawn. Moving it afterwards leaves a window in which a test that
//! allocates immediately — which is precisely the test this exists to bound — has already escaped
//! the limit. The hook runs between `fork` and `exec` in a process that may hold locks belonging to
//! threads that no longer exist, so it does exactly one thing: a `write` of two bytes to a file
//! descriptor that was opened before the fork. No allocation, no formatting, no locking. Writing
//! `0` rather than a pid is what makes the formatting unnecessary; the kernel reads it as "the
//! process doing the writing".
//!
//! Availability is the real limitation. The host must use the unified hierarchy, and cargo-gamma's
//! own cgroup must both permit creating children and be allowed to hand the memory controller to
//! them. cgroup v2 refuses to delegate a controller out of a cgroup that still holds processes of
//! its own, so when the controller is not already delegated this process moves itself into a
//! subgroup first — the arrangement `systemd-run --user --scope -p Delegate=yes` expects of anyone
//! it delegates to. Where none of that is possible, [`root`] says so with the reason, and the run
//! reports that rather than claiming a limit it never installed.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;

use super::memory::Usage;

/// Where the unified hierarchy is mounted, on every distribution that uses it.
const MOUNT: &str = "/sys/fs/cgroup";

/// The file naming the cgroup a process belongs to.
const SELF_CGROUP: &str = "/proc/self/cgroup";

/// The subgroup this process moves itself into when the controller has to be delegated.
const SUPERVISOR: &str = "gamma.supervisor";

/// What is written to `cgroup.procs` to move the writing process into that cgroup.
///
/// Two bytes rather than a formatted pid, so that the `pre_exec` hook needs neither a buffer nor a
/// conversion; a hook that formatted anything would be allocating between `fork` and `exec`.
const MOVE_SELF: &[u8] = b"0\n";

/// How many passes are made over a cgroup's process list when emptying it.
///
/// More than one because a process can be spawned into the cgroup while the list is being walked,
/// and few because a cgroup that is still filling after a handful of passes belongs to something
/// other than this run and is not going to become empty by being asked again.
const MIGRATION_ATTEMPTS: u32 = 4;

/// How many times removing a spent cgroup is retried before it is left behind.
///
/// Removal fails while the cgroup still holds a process, which after a normal run means something
/// the test spawned outlived it. Waiting briefly collects the ordinary case; waiting indefinitely
/// would hand a run's pace to whatever a test forgot to kill.
const REMOVAL_ATTEMPTS: u32 = 20;

/// How long to wait between attempts at removing a spent cgroup.
const REMOVAL_PAUSE: Duration = Duration::from_millis(10);

/// Where per-invocation cgroups are created, or the reason there is nowhere to create them.
static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// Distinguishes concurrently created cgroups, since workers create them from several threads.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Returns the directory per-invocation cgroups are created under, or why there is none.
///
/// Settled once. The work behind it includes creating a cgroup and possibly moving this process
/// into a subgroup of its own, neither of which is worth repeating, and a run that cannot have a
/// memory ceiling deserves one clear explanation rather than one per mutant.
pub(super) fn root() -> Result<&'static Path, &'static str> {
    match ROOT.get_or_init(discover) {
        Ok(path) => Ok(path.as_path()),
        Err(reason) => Err(reason.as_str()),
    }
}

/// Works out whether this process can create memory-controlled cgroups, and where.
fn discover() -> Result<PathBuf, String> {
    let own = own()?;
    let root = delegate(&own)?;

    probe(&root)?;

    Ok(root)
}

/// The cgroup this process is currently in.
fn own() -> Result<PathBuf, String> {
    let listed = fs::read_to_string(SELF_CGROUP).map_err(|cause| format!("`{SELF_CGROUP}` could not be read: {cause}"))?;

    // The unified hierarchy is always the entry with an empty controller list and hierarchy id
    // zero. Anything else in this file belongs to a v1 hierarchy, which has no aggregate memory
    // accounting worth using.
    let relative = listed
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| "this host is not using the cgroup v2 unified hierarchy".to_owned())?;

    let path = Path::new(MOUNT).join(relative.trim().trim_start_matches('/'));

    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "cargo-gamma's own cgroup is not visible at `{}`, which happens when the unified \
             hierarchy is mounted elsewhere or when this process is in a container that hides it",
            path.display()
        ))
    }
}

/// Ensures children of `own` will have the memory controller, moving this process if it must.
fn delegate(own: &Path) -> Result<PathBuf, String> {
    if lists(own, "cgroup.subtree_control", "memory") {
        return Ok(own.to_owned());
    }

    if !lists(own, "cgroup.controllers", "memory") {
        return Err(format!(
            "the memory controller is not available to cargo-gamma's cgroup `{}`; a delegated \
             cgroup with the memory controller is needed, as `systemd-run --user --scope -p \
             Delegate=yes` provides",
            own.display()
        ));
    }

    if enable(own).is_ok() {
        return Ok(own.to_owned());
    }

    // cgroup v2 will not hand a controller to the children of a cgroup that still holds processes,
    // and this process is one of them. Moving into a subgroup is what a delegated unit is expected
    // to do, and it leaves the delegation boundary itself free to distribute controllers.
    let supervisor = own.join(SUPERVISOR);

    fs::create_dir_all(&supervisor).map_err(|cause| {
        format!(
            "`{}` could not be created: {cause}. Creating a cgroup there needs one delegated to this \
             process, as `systemd-run --user --scope -p Delegate=yes` provides",
            supervisor.display()
        )
    })?;

    vacate(own, &supervisor)?;

    enable(own).map_err(|cause| {
        format!(
            "the memory controller could not be delegated to the children of `{}`: {cause}. \
             This usually means the cgroup is shared with processes that are not part of this run",
            own.display()
        )
    })?;

    Ok(own.to_owned())
}

/// Moves every process out of `own` and into `supervisor`.
///
/// This process is moved first and by name-free means, then anything else found in the cgroup. The
/// others matter because cargo-gamma is rarely alone: `cargo gamma` leaves the cargo that launched
/// it in the same cgroup, and one leftover process is enough for the kernel to refuse the
/// delegation. They are the run's own ancestors and siblings within a cgroup this process has
/// already proved it may write to, and a cgroup is not a security or scheduling boundary they can
/// notice being moved within — they keep the same session, the same limits and the same parent.
///
/// A process that exits between being listed and being written is not an error; it has left the
/// cgroup by the only other means available. The listing is re-read because moving one process can
/// let another be reaped or spawned, and the kernel judges the cgroup empty only at the moment of
/// the write.
///
/// # Errors
///
/// Returns the reason when the cgroup could not be emptied, which leaves the caller to report that
/// no ceiling was installed rather than to proceed as though one had been.
fn vacate(own: &Path, supervisor: &Path) -> Result<(), String> {
    let procs = supervisor.join("cgroup.procs");

    fs::write(&procs, MOVE_SELF).map_err(|cause| format!("`{}` could not be written: {cause}", procs.display()))?;

    for _attempt in 0..MIGRATION_ATTEMPTS {
        let listed = fs::read_to_string(own.join("cgroup.procs"))
            .map_err(|cause| format!("`{}` could not be read: {cause}", own.display()))?;

        let mut remaining = 0_usize;

        for pid in listed.split_ascii_whitespace() {
            remaining = remaining.saturating_add(1);

            let _moved = fs::write(&procs, pid);
        }

        if remaining == 0 {
            return Ok(());
        }
    }

    Err(format!(
        "`{}` still holds processes that are not part of this run, and cgroup v2 will not hand the          memory controller to the children of a cgroup that is not empty",
        own.display()
    ))
}

/// Asks a cgroup to hand the memory controller to its children.
fn enable(own: &Path) -> io::Result<()> {
    fs::write(own.join("cgroup.subtree_control"), "+memory")
}

/// Whether one of a cgroup's space-separated interface files names `wanted`.
fn lists(path: &Path, file: &str, wanted: &str) -> bool {
    fs::read_to_string(path.join(file))
        .is_ok_and(|text| text.split_ascii_whitespace().any(|name| name == wanted))
}

/// Confirms that a child cgroup can actually be created here and offers what the run needs.
///
/// Both files are checked because they answer different questions and fail on different hosts:
/// `memory.max` is what bounds a mutant, and `memory.peak` — which arrived in Linux 5.19 — is what
/// the baseline measures in order to choose that bound.
fn probe(root: &Path) -> Result<(), String> {
    let path = root.join(format!("gamma.probe.{}", std::process::id()));

    fs::create_dir(&path).map_err(|cause| {
        format!(
            "a child cgroup could not be created under `{}`: {cause}. Creating one needs a \
             delegated cgroup, as `systemd-run --user --scope -p Delegate=yes` provides",
            root.display()
        )
    })?;

    let missing = ["memory.max", "memory.peak"].into_iter().find(|name| !path.join(name).exists());
    let _removed = fs::remove_dir(&path);

    missing.map_or(Ok(()), |name| {
        Err(format!(
            "a child cgroup created under `{}` has no `{name}`, so this kernel cannot \
             {} a process tree's memory",
            root.display(),
            if name == "memory.peak" { "measure" } else { "bound" }
        ))
    })
}

/// One invocation's accounting boundary: a cgroup leaf holding a test binary and its descendants.
#[derive(Debug)]
pub(super) struct Cgroup {
    /// The leaf's directory, removed when this is dropped.
    path: PathBuf,
}

impl Cgroup {
    /// Creates a leaf, optionally bounded, ready for a child to move itself into.
    pub(super) fn create(limit: Option<u64>) -> Result<Self, String> {
        let root = root().map_err(str::to_owned)?;
        let name = format!("gamma.{}.{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed));
        let path = root.join(name);

        fs::create_dir(&path).map_err(|cause| format!("`{}` could not be created: {cause}", path.display()))?;

        let group = Self { path };

        // An OOM that killed one process of the tree and left the rest running would turn a mutant
        // that exhausted memory into a suite failing for an unrelated-looking reason, with the
        // survivors still holding locks in the scratch tree. Best effort, because a kernel without
        // it still enforces the ceiling; it merely enforces it less tidily.
        let _grouped = group.set("memory.oom.group", "1");

        if let Some(limit) = limit {
            group.set("memory.max", &limit.to_string())?;

            // Capping resident memory alone turns a crash into swap thrashing: the workload stays
            // under `memory.max` by pushing pages to disk and the machine becomes unusable while
            // the mutant is technically within its budget. Best effort, since a host without swap
            // accounting has nothing to disable.
            let _unswapped = group.set("memory.swap.max", "0");
        }

        Ok(group)
    }

    /// Arranges for the child to place itself in this cgroup before it executes.
    pub(super) fn arm(&self, command: &mut Command) -> Result<(), String> {
        let procs = self.path.join("cgroup.procs");
        let file = OpenOptions::new()
            .write(true)
            .open(&procs)
            .map_err(|cause| format!("`{}` could not be opened: {cause}", procs.display()))?;

        let join = move || {
            let fd = file.as_raw_fd();

            // SAFETY: the descriptor is open for writing and owned by the `File` this closure
            // holds, so it cannot have been closed or reused between the open above and this
            // write. The buffer is a `'static` constant and its length is passed exactly, so
            // `write` reads only initialized memory that outlives the call.
            let written = unsafe { libc::write(fd, MOVE_SELF.as_ptr().cast(), MOVE_SELF.len()) };

            // A child that could not join the cgroup must not go on to exec: it would run
            // unaccounted and unbounded, which is the one outcome this whole mechanism exists to
            // prevent. Failing the spawn instead is reported to the caller as a setup failure
            // rather than as a verdict about any mutant.
            if usize::try_from(written).is_ok_and(|count| count == MOVE_SELF.len()) {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };

        // SAFETY: `pre_exec` requires that the closure be safe to run between `fork` and `exec` in
        // a child that has only the forking thread and may hold locks the other threads left
        // behind. `join` allocates nothing, takes no lock, and calls exactly one function —
        // `write`, which POSIX lists as async-signal-safe. The descriptor it writes to was opened
        // before the fork, so no path is resolved and no file is opened in the child, and
        // `as_raw_fd` reads a field rather than performing a call at all. `io::Error` from a raw
        // errno allocates nothing either.
        let _armed = unsafe { command.pre_exec(join) };

        Ok(())
    }

    /// What this cgroup accounted for, once the subtree has finished.
    pub(super) fn usage(&self) -> Usage {
        Usage {
            peak: self.get("memory.peak").and_then(|text| text.trim().parse().ok()),
            exhausted: self.oom_killed(),
        }
    }

    /// Kills everything in the cgroup, including anything that left the process group.
    pub(super) fn kill(&self) {
        let _killed = self.set("cgroup.kill", "1");
    }

    /// Whether the kernel reported killing this workload for reaching its ceiling.
    ///
    /// Only `oom` and `oom_kill` count. A `max` event says an allocation reached the ceiling, which
    /// happens whenever reclaim is doing its job, and a suite that allocated hard and then passed
    /// is not a mutant the tests caught. The local file is preferred because it counts this cgroup
    /// alone; the aggregate one is the fallback for kernels that do not offer it.
    fn oom_killed(&self) -> bool {
        let events = self
            .get("memory.events.local")
            .or_else(|| self.get("memory.events"))
            .unwrap_or_default();

        events.lines().any(|line| {
            let mut fields = line.split_ascii_whitespace();
            let name = fields.next();
            let count = fields.next().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);

            matches!(name, Some("oom" | "oom_kill")) && count > 0
        })
    }

    /// Writes one of the cgroup's interface files.
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let path = self.path.join(name);

        fs::write(&path, value).map_err(|cause| format!("`{}` could not be written: {cause}", path.display()))
    }

    /// Reads one of the cgroup's interface files, if the kernel offers it.
    fn get(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.path.join(name)).ok()
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // A cgroup that still holds a process cannot be removed. After a normal run that means
        // something the test spawned outlived it, which is rare; leaving the directory behind is
        // untidy but harmless, and blocking the run until an orphan decides to exit would not be.
        for _attempt in 0..REMOVAL_ATTEMPTS {
            if fs::remove_dir(&self.path).is_ok() {
                return;
            }

            thread::sleep(REMOVAL_PAUSE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether this host can actually create memory-controlled cgroups.
    ///
    /// Delegation is not universal: containers, CI runners and ordinary unprivileged systemd
    /// sessions all differ. Tests that need the real thing skip rather than fail, so that a
    /// checkout is not red on a machine that simply cannot offer the feature.
    fn delegated() -> bool {
        root().is_ok()
    }

    /// Whatever this host answers, the detection is decisive and repeatable.
    #[test]
    fn capability_detection_settles_on_one_answer() {
        // A detection that answered differently on the second call would install a limit for some
        // mutants and not others, and the run would report a protection it only partly had.
        let first = root().map(Path::to_path_buf);
        let second = root().map(Path::to_path_buf);

        assert_eq!(first, second);
    }

    /// An unsupported host explains itself in terms of the machine, not of this tool.
    #[test]
    fn an_undelegated_host_says_what_is_missing() {
        if let Err(reason) = root() {
            assert!(
                reason.contains("cgroup") || reason.contains("memory") || reason.contains("kernel"),
                "{reason}"
            );
        }
    }

    /// A cgroup leaf measures the memory of the child that ran in it.
    #[test]
    fn a_leaf_measures_what_the_child_allocated() {
        if !delegated() {
            return;
        }

        let group = Cgroup::create(None).expect("a leaf is created on a delegated host");
        let mut command = Command::new("sh");

        // Allocated and then touched, because a mapping nothing writes to is never resident and
        // would leave the peak unchanged.
        let _ = command.args(["-c", "dd if=/dev/zero of=/dev/null bs=1M count=64 2>/dev/null"]);

        group.arm(&mut command).expect("the hook is installed");

        let mut child = command.spawn().expect("spawn");

        assert!(child.wait().expect("wait").success());

        let usage = group.usage();

        assert!(usage.peak.is_some_and(|peak| peak > 0), "{usage:?}");
        assert!(!usage.exhausted, "{usage:?}");
    }

    /// A child that passes the ceiling is killed by the kernel and reported as such.
    #[test]
    fn a_child_that_passes_the_ceiling_is_reported_as_exhausted() {
        if !delegated() {
            return;
        }

        // The distinction this asserts is the whole point of reading the cgroup's own events: a
        // process killed by the kernel for reaching the ceiling and a test that simply failed both
        // exit non-zero, and only one of them is a memory verdict.
        //
        // The allocation is a shared-memory file rather than a disk one, because page cache backed
        // by a disk is reclaimable and would keep the workload under the ceiling forever instead of
        // crossing it.
        let fill = format!("/dev/shm/gamma-fill.{}", std::process::id());
        let group = Cgroup::create(Some(32 * 1024 * 1024)).expect("a bounded leaf is created");
        let mut command = Command::new("sh");
        let _ = command.args(["-c", &format!("dd if=/dev/zero of={fill} bs=1M count=256 2>/dev/null")]);

        group.arm(&mut command).expect("the hook is installed");

        let mut child = command.spawn().expect("spawn");
        let _status = child.wait().expect("wait");
        let usage = group.usage();
        let _removed = fs::remove_file(&fill);

        assert!(usage.exhausted, "{usage:?}");
    }

    /// A cgroup that was never used is removed when it is dropped.
    #[test]
    fn a_spent_leaf_is_removed() {
        if !delegated() {
            return;
        }

        // One directory per invocation and thousands of invocations per run: leaking them would
        // eventually reach the kernel's own limit on how many descendants a cgroup may have.
        let group = Cgroup::create(None).expect("a leaf is created on a delegated host");
        let path = group.path.clone();

        drop(group);

        assert!(!path.exists());
    }
}
