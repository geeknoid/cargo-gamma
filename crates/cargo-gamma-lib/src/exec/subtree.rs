//! Killing a test binary and everything it spawned, and accounting for what it used.
//!
//! Killing the process a run started is not enough. A test that shells out to a server, a database
//! or another build leaves those behind when the harness above them is cut off, and they take two
//! things with them: file locks inside the scratch tree, which turn the next run into a failure
//! that has nothing to do with any mutant, and inherited pipe handles, which keep whoever is
//! reading this tool's output from ever seeing end of file. A run that ends with a hung consumer is
//! worse than one that ends with a wrong verdict, because nobody can even see the verdict.
//!
//! Both platforms have a way to say "this process and everything descended from it" — a process
//! group on Unix and a job object on Windows — but neither is reachable from `std`, so each is
//! reached here through its own thin unsafe wrapper and behind one shared interface.
//!
//! The same boundary is where memory is accounted for, because it is the only place that knows the
//! whole subtree rather than the one process at its root. A [`Request`] passed to [`contain`] asks
//! for measurement, a ceiling, or neither; [`Subtree::usage`] answers once the subtree is gone. On
//! Windows the job object that kills the subtree also carries the limit and the accounting; on
//! Linux the process group cannot carry either, so a cgroup leaf does — see [`super::cgroup`].
//!
//! Containment cuts one tie that used to do this work by accident. A terminal delivers `Ctrl-C` to
//! the whole foreground process group, so a child sharing this process's group died with it for
//! free; a child leading its own group does not. Windows keeps the guarantee on its own, because a
//! job set to die with its last handle dies when this process does, however it does. Unix has to
//! ask, so it does — see [`interrupt`].

use std::process::{Child, Command};

use super::memory::{Request, Usage};

/// The resource control installed for one spawn, handed to [`Subtree::adopt`] afterwards.
///
/// Created before the child exists because both halves of it have to be: a cgroup's `cgroup.procs`
/// must be open before the fork for the child to move itself, and a job object's limits must be
/// configured before a process is assigned to it, or the process runs briefly unbounded.
#[derive(Debug, Default)]
pub(super) struct Guard {
    /// The cgroup leaf the child will place itself in, on the platform that has them.
    #[cfg(target_os = "linux")]
    cgroup: Option<super::cgroup::Cgroup>,

    /// The job the child will be assigned to, on the platform that has them.
    #[cfg(windows)]
    job: Option<Job>,
}

/// Arranges for a child's descendants to be killable along with it, and accounted for.
///
/// Called before spawning, because on Unix the containment has to be requested as part of the
/// spawn itself.
///
/// # Errors
///
/// Returns the reason when `request` asked for measurement or a ceiling and the platform could not
/// install one. A run that asked to be protected and silently was not is the failure this reports
/// rather than swallows: the user would believe the machine was bounded, and find out otherwise
/// only when it was not.
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        clippy::needless_pass_by_ref_mut,
        reason = "only the cgroup path reaches into the command, and the signature is shared"
    )
)]
pub(super) fn contain(command: &mut Command, request: Request) -> Result<Guard, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        interrupt::arm();

        // The child leads its own process group, so a later signal to the negated group id reaches
        // every descendant that has not deliberately left the group. Without this the child shares
        // this process's group, and signalling the group would kill the run itself.
        let _ = command.process_group(0);
    }

    #[cfg(target_os = "linux")]
    {
        if !request.wanted() {
            return Ok(Guard { cgroup: None });
        }

        let cgroup = super::cgroup::Cgroup::create(request.limit)?;

        cgroup.arm(command)?;

        Ok(Guard { cgroup: Some(cgroup) })
    }

    #[cfg(windows)]
    {
        #[expect(
            clippy::no_effect_underscore_binding,
            reason = "the command is only reached into on Linux, and an unused parameter is a warning of its own"
        )]
        let _unused = command;

        match Job::create(request.limit) {
            Some(job) => Ok(Guard { job: Some(job) }),
            // A job that could not be created has always meant a subtree this run cannot kill,
            // which it tolerates because killing one process is better than killing none. It
            // cannot tolerate it once the job is also the memory boundary.
            None if request.wanted() => {
                Err("a Windows job object could not be created, so this test binary's memory could not be accounted for".to_owned())
            }
            None => Ok(Guard { job: None }),
        }
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        #[expect(
            clippy::no_effect_underscore_binding,
            reason = "the command is only reached into on Linux, and an unused parameter is a warning of its own"
        )]
        let _unused = command;

        if request.wanted() {
            return Err(super::memory::support().unwrap_or_else(|reason| reason));
        }

        Ok(Guard::default())
    }
}

/// A handle on a spawned child's whole subtree.
///
/// Created from an already-spawned child, and used once when the run decides the mutant has hung.
#[derive(Debug)]
pub(super) struct Subtree {
    /// The process group the child leads, on the platform that has them.
    ///
    /// Absent when the child's id does not fit a signal's idea of one, which cannot happen on any
    /// real system but is not worth an unchecked conversion to assume.
    #[cfg(unix)]
    group: Option<i32>,

    /// Where this child's group sits in the list an interrupt walks, released when it is killed.
    #[cfg(unix)]
    slot: Option<usize>,

    /// The cgroup leaf accounting for this subtree, when one was asked for.
    #[cfg(target_os = "linux")]
    cgroup: Option<super::cgroup::Cgroup>,

    /// The job the child was placed in, on the platform that has them.
    #[cfg(windows)]
    job: Option<Job>,
}

impl Subtree {
    /// Takes hold of a freshly spawned child, together with whatever [`contain`] set up for it.
    ///
    /// # Errors
    ///
    /// Returns the reason when the guard could not be applied to the child that was just spawned.
    /// The caller owns a live process at that point and has to end it, since the alternative is a
    /// test binary running outside the accounting the run believes it is inside.
    #[cfg_attr(
        unix,
        expect(
            clippy::unnecessary_wraps,
            reason = "nothing can fail on Unix, where the guard is installed before the spawn, \
                      but assigning a Windows job object can, and the signature is shared"
        )
    )]
    pub(super) fn adopt(child: &Child, guard: Guard) -> Result<Self, String> {
        #[cfg(unix)]
        {
            let group = i32::try_from(child.id()).ok();

            // Registered so that an interrupt can reach it. A run cut off at the terminal used to
            // take its children with it because they shared its group; now that they do not, the
            // only thing that still knows about them is this list.
            let slot = group.and_then(interrupt::watch);

            #[cfg(target_os = "linux")]
            {
                Ok(Self { group, slot, cgroup: guard.cgroup })
            }

            #[cfg(not(target_os = "linux"))]
            {
                drop(guard);

                Ok(Self { group, slot })
            }
        }

        #[cfg(windows)]
        {
            if let Some(job) = guard.job.as_ref()
                && !job.assign(child)
            {
                return Err("a Windows job object could not be given the test binary it was created for".to_owned());
            }

            Ok(Self { job: guard.job })
        }

        #[cfg(not(any(unix, windows)))]
        {
            drop(child);
            drop(guard);

            Ok(Self {})
        }
    }

    /// What the platform accounted for, read once the subtree has finished.
    pub(super) fn usage(&self) -> Usage {
        #[cfg(target_os = "linux")]
        {
            self.cgroup.as_ref().map_or_else(Usage::default, super::cgroup::Cgroup::usage)
        }

        #[cfg(windows)]
        {
            self.job.as_ref().map_or_else(Usage::default, Job::usage)
        }

        #[cfg(not(any(target_os = "linux", windows)))]
        {
            Usage::default()
        }
    }

    /// Kills the child and every process descended from it.
    ///
    /// Falls back to killing the child alone whenever the subtree cannot be reached, because a run
    /// that cut off one process is still better than one that cut off none.
    pub(super) fn kill(&self, child: &mut Child) {
        // The cgroup reaches further than the process group does: a descendant that called
        // `setsid` has left the group but cannot leave the cgroup, so this goes first where it
        // exists.
        #[cfg(target_os = "linux")]
        if let Some(cgroup) = self.cgroup.as_ref() {
            cgroup.kill();
        }

        // Signalling the group has to come first: killing the leader on its own leaves the group
        // without one, and the descendants are then reparented and unreachable.
        #[cfg(unix)]
        if let Some(group) = self.group {
            // SAFETY: `killpg` takes a group id and a signal and touches no memory. The group is
            // the one `contain` created for this child, so nothing outside this run's own subtree
            // can be in it. A group that has already exited yields `ESRCH`, which is reported
            // through the return value rather than through anything unsound.
            let _sent = unsafe { libc::killpg(group, libc::SIGKILL) };
        }

        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }

        // Always, on every platform: the group or job may not have covered the child — the id
        // could not be converted, the job could not be created — and in any case this is what
        // reaps the handle so that `wait` returns.
        let _killed = child.kill();
    }
}

#[cfg_attr(
    not(unix),
    expect(
        clippy::empty_drop,
        reason = "only the interrupt list needs releasing, and only Unix has one; the members that \
                  do own something elsewhere release it themselves"
    )
)]
impl Drop for Subtree {
    fn drop(&mut self) {
        // A child that exited on its own is still in the list, and its group id will eventually be
        // handed to something else. Releasing the slot is what keeps an interrupt from signalling
        // a stranger.
        #[cfg(unix)]
        if let Some(slot) = self.slot {
            interrupt::forget(slot);
        }
    }
}

#[cfg(unix)]
mod interrupt {
    //! Taking the children along when this process is interrupted.
    //!
    //! Everything here runs, or may run, inside a signal handler, so it is written to the rules
    //! that implies: no allocation, no locks, and no calls that are not async-signal-safe. The
    //! registry is therefore a fixed array of atomics rather than the obvious `Vec` behind a
    //! `Mutex`, which a handler could deadlock against the very thread it interrupted.

    use core::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Once;

    /// How many children can be watched at once.
    ///
    /// A run has one live child per worker, and workers are capped at the machine's parallelism.
    /// A slot that cannot be claimed costs nothing but the guarantee this module adds, so the size
    /// is generous rather than exact.
    const SLOTS: usize = 1024;

    /// The empty slot marker. No process group is ever zero from this process's point of view.
    const EMPTY: i32 = 0;

    /// The groups an interrupt should take with it.
    static WATCHED: [AtomicI32; SLOTS] = [const { AtomicI32::new(EMPTY) }; SLOTS];

    /// Guards installing the handlers, which must happen exactly once.
    static ARMED: Once = Once::new();

    /// Installs the handlers, the first time anything is contained.
    pub(super) fn arm() {
        ARMED.call_once(|| {
            // C spells a signal handler as an integer, which is what `sighandler_t` is, so the
            // pointer has to be widened into one. There is no other way to say this.
            #[expect(clippy::fn_to_numeric_cast_any, reason = "the C signal API takes a handler as an integer")]
            let target: libc::sighandler_t = handler as extern "C" fn(i32) as usize;

            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                // SAFETY: `signal` installs a handler for a valid signal number. The handler is a
                // plain `extern "C"` function that calls only async-signal-safe functions, which is
                // the whole contract this has to meet.
                let _previous = unsafe { libc::signal(signal, target) };
            }
        });
    }

    /// Starts watching a group, returning the slot it took.
    pub(super) fn watch(group: i32) -> Option<usize> {
        WATCHED.iter().position(|slot| {
            slot.compare_exchange(EMPTY, group, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        })
    }

    /// Stops watching whatever is in a slot.
    pub(super) fn forget(slot: usize) {
        if let Some(entry) = WATCHED.get(slot) {
            entry.store(EMPTY, Ordering::Release);
        }
    }

    /// Kills every watched group and then dies of the signal that arrived.
    ///
    /// Re-raising rather than exiting is what makes the wait status right: a shell reports an
    /// interrupted process by the signal that killed it, and a process that quietly exits instead
    /// looks to every script above it like one that decided to stop.
    extern "C" fn handler(signal: i32) {
        for entry in &WATCHED {
            let group = entry.swap(EMPTY, Ordering::AcqRel);

            if group != EMPTY {
                // SAFETY: `killpg` is async-signal-safe, takes a group id and a signal, and touches
                // no memory. The group is one this run created.
                let _sent = unsafe { libc::killpg(group, libc::SIGKILL) };
            }
        }

        // SAFETY: restoring the default disposition is async-signal-safe, and doing it first is
        // what keeps the re-raise below from re-entering this handler forever.
        let _previous = unsafe { libc::signal(signal, libc::SIG_DFL) };

        // SAFETY: `raise` is async-signal-safe and touches no memory. The disposition is now the
        // default, so this ends the process with the status the signal implies.
        let _raised = unsafe { libc::raise(signal) };
    }
}

#[cfg(windows)]
mod job {
    use core::ffi::c_void;
    use core::mem::{self, size_of};
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };

    use crate::exec::memory::Usage;

    /// A Windows job object holding one child and everything it goes on to spawn.
    #[derive(Debug)]
    pub(super) struct Job {
        /// The job handle, closed when this is dropped.
        handle: HANDLE,

        /// The aggregate committed memory the job was limited to, when it was limited at all.
        limit: Option<u64>,
    }

    // SAFETY: a job handle is a kernel object reference with no thread affinity, and every use of
    // it here goes through a Win32 call that is itself thread-safe.
    unsafe impl Send for Job {}

    // SAFETY: as above; nothing in this type is mutated after construction.
    unsafe impl Sync for Job {}

    impl Job {
        /// Creates a job, configured before any process is put in it.
        ///
        /// The limit is installed here rather than after [`Self::assign`] because a job whose limit
        /// arrives second is a job the child spent its first instants outside of, and the test this
        /// exists to bound is exactly the one that allocates immediately.
        pub(super) fn create(limit: Option<u64>) -> Option<Self> {
            // SAFETY: both arguments are null, which the API documents as "unnamed job with
            // default security". A failure is reported as a null handle rather than by any other
            // means.
            let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };

            if handle.is_null() {
                return None;
            }

            let job = Self { handle, limit };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                // SAFETY: the structure is plain old data whose every field is an integer, and it
                // is fully overwritten below except for the reserved fields, which the API expects
                // to be zero.
                unsafe { mem::zeroed() };

            // Everything in the job dies when the last handle to it closes, which covers the run
            // being killed itself: the handle goes with the process, and the subtree with it.
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            if let Some(limit) = limit {
                // The job-wide limit is the one worth having: a per-process limit would let a test
                // that spawns helpers reach any total it liked, which is the shape of runaway
                // allocation this is here to stop.
                let Ok(bytes) = usize::try_from(limit) else {
                    return None;
                };

                limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                limits.JobMemoryLimit = bytes;
            }

            // SAFETY: the handle is a live job, the class matches the structure being passed, and
            // the length is that structure's own size.
            let set = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    core::ptr::from_mut(&mut limits).cast::<c_void>(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                )
            };

            if set == 0 {
                return None;
            }

            Some(job)
        }

        /// Puts an already-spawned child in the job.
        ///
        /// The child is assigned after it exists rather than before it starts, because `std` gives
        /// no way to spawn a process suspended. A grandchild spawned in the moment between the two
        /// escapes the job, which is a far smaller window than the whole life of a test and the
        /// best that can be had without owning the spawn.
        pub(super) fn assign(&self, child: &Child) -> bool {
            // SAFETY: the handle is a live job and the second argument is the child's own process
            // handle, which `Child` keeps open for as long as it lives.
            let assigned = unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle().cast::<c_void>()) };

            assigned != 0
        }

        /// What the job accounted for, read once its processes have finished.
        ///
        /// A job whose limit fires refuses allocations rather than necessarily killing anything at
        /// once, so exhaustion is read from the job's own accounting rather than inferred from an
        /// exit status that any ordinary failing test could also produce.
        pub(super) fn usage(&self) -> Usage {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                // SAFETY: the structure is plain old data whose every field is an integer, and the
                // call below either overwrites it entirely or is reported as having failed.
                unsafe { mem::zeroed() };
            let mut returned: u32 = 0;

            // SAFETY: the handle is a live job, the class matches the structure being written into,
            // and the length is that structure's own size. The final argument is a live `u32`.
            let queried = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectExtendedLimitInformation,
                    core::ptr::from_mut(&mut limits).cast::<c_void>(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                    core::ptr::from_mut(&mut returned),
                )
            };

            if queried == 0 {
                return Usage::default();
            }

            let peak = u64::try_from(limits.PeakJobMemoryUsed).ok();

            Usage {
                peak,
                exhausted: self
                    .limit
                    .zip(peak)
                    .is_some_and(|(limit, peak)| peak >= limit),
            }
        }

        /// Kills everything in the job.
        pub(super) fn terminate(&self) {
            // SAFETY: the handle is a live job. The exit code is arbitrary and is never read, since
            // a killed mutant's status is decided by the run rather than by the process.
            let _terminated = unsafe { TerminateJobObject(self.handle, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle was created by `CreateJobObjectW`, is closed exactly once because
            // this type is not `Clone`, and is not used afterwards.
            let _closed = unsafe { CloseHandle(self.handle) };
        }
    }
}

#[cfg(windows)]
use job::Job;

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that asks for nothing gets containment without an accounting boundary.
    #[test]
    fn a_run_that_asks_for_no_accounting_reports_no_usage() {
        // Metering is opt-in, and a run that did not ask for it must not be told a peak of zero as
        // though it were a measurement.
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "true" });

        if cfg!(windows) {
            let _ = command.args(["/c", "exit 0"]);
        }

        let guard = contain(&mut command, Request::default()).expect("containment");
        let mut child = command.spawn().expect("spawn");
        let subtree = Subtree::adopt(&child, guard).expect("adoption");

        let _status = child.wait().expect("wait");

        assert_eq!(subtree.usage(), Usage::default());
    }

    /// Asking for accounting on a host that cannot provide it fails rather than running anyway.
    #[test]
    fn asking_for_accounting_a_host_cannot_provide_fails_the_spawn() {
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "true" });

        if cfg!(windows) {
            let _ = command.args(["/c", "exit 0"]);
        }

        // Either the host can meter and this succeeds, or it cannot and the caller is told why.
        // What must never happen is the third thing: a run that believes it is bounded, is not,
        // and finds out when the machine runs out of memory.
        match contain(&mut command, Request { meter: true, limit: None }) {
            Ok(_guard) => super::super::memory::support().expect("metering succeeded, so it is supported"),
            Err(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
        }
    }

    #[test]
    fn a_contained_child_can_still_be_run_and_waited_for() {
        // Containment must not change what a normal run does, only what a kill reaches.
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "true" });

        if cfg!(windows) {
            let _ = command.args(["/c", "exit 0"]);
        }

        let guard = contain(&mut command, Request::default()).expect("containment");

        let mut child = command.spawn().expect("spawn");
        let _subtree = Subtree::adopt(&child, guard).expect("adoption");

        assert!(child.wait().expect("wait").success());
    }

    /// A metered child's peak is reported through the subtree, on a host that can measure one.
    #[test]
    #[cfg(unix)]
    fn a_metered_subtree_reports_what_it_used() {
        if super::super::memory::support().is_err() {
            return;
        }

        let mut command = Command::new("sh");
        let _ = command.args(["-c", "dd if=/dev/zero of=/dev/null bs=1M count=32 2>/dev/null"]);

        let guard = contain(&mut command, Request { meter: true, limit: None }).expect("containment");
        let mut child = command.spawn().expect("spawn");
        let subtree = Subtree::adopt(&child, guard).expect("adoption");

        let _status = child.wait().expect("wait");
        let usage = subtree.usage();

        // The measurement is what every derived ceiling is built from, so a boundary that
        // installed itself and then measured nothing would be worse than none at all.
        assert!(usage.peak.is_some(), "{usage:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_contained_child_leads_its_own_process_group() {
        // Which is what makes the group signal reach the descendants without reaching this run.
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "sleep 30"]);

        let guard = contain(&mut command, Request::default()).expect("containment");

        let mut child = command.spawn().expect("spawn");
        let id = i32::try_from(child.id()).expect("pid");

        // SAFETY: `getpgid` reads the group of an existing process and touches no memory.
        let group = unsafe { libc::getpgid(id) };

        assert_eq!(group, id);

        Subtree::adopt(&child, guard).expect("adoption").kill(&mut child);

        let _reaped = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn killing_the_subtree_reaches_a_grandchild() {
        // The whole point: a test that spawned something is not cut off by killing the harness.
        let mut command = Command::new("sh");

        // The grandchild outlives its parent deliberately, which is exactly the shape that leaves
        // an orphan holding the scratch tree and the caller's pipe.
        let _ = command.args(["-c", "sleep 30 & echo $!; exec sleep 30"]);
        let _ = command.stdout(std::process::Stdio::piped());

        let guard = contain(&mut command, Request::default()).expect("containment");

        let mut child = command.spawn().expect("spawn");
        let subtree = Subtree::adopt(&child, guard).expect("adoption");
        let mut text = String::new();

        {
            use std::io::Read as _;

            let mut pipe = child.stdout.take().expect("pipe");
            let mut byte = [0_u8; 1];

            while matches!(pipe.read(&mut byte), Ok(1)) {
                if byte[0] == b'\n' {
                    break;
                }

                text.push(char::from(byte[0]));
            }
        }

        let grandchild: i32 = text.trim().parse().expect("grandchild pid");

        subtree.kill(&mut child);

        let _reaped = child.wait();

        // Signal zero asks whether the process can be signalled rather than signalling it, which is
        // how liveness is checked without disturbing anything.
        for _attempt in 0..100 {
            // SAFETY: `kill` with signal zero performs a permission and existence check only.
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                return;
            }

            std::thread::sleep(core::time::Duration::from_millis(10));
        }

        panic!("the grandchild outlived the kill");
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_takes_the_children_with_it() {
        // A child in its own process group no longer dies with the terminal's `Ctrl-C`, so the run
        // has to take it along deliberately. Checked in a subprocess, because the handler ends by
        // re-raising the signal and killing whoever is running it.
        let program = std::env::args().next().expect("this test binary");
        let mut child = Command::new(&program)
            .args(["--exact", "--nocapture", "exec::subtree::tests::spawns_a_child_then_interrupts_itself"])
            .env("GAMMA_INTERRUPT_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");

        let mut text = String::new();

        {
            use std::io::Read as _;

            let _read = child.stdout.take().expect("pipe").read_to_string(&mut text);
        }

        let _status = child.wait();
        let reported: i32 = text
            .lines()
            .find_map(|line| line.strip_prefix("grandchild "))
            .and_then(|rest| rest.trim().parse().ok())
            .expect("the inner test reports the pid it spawned");

        for _attempt in 0..200 {
            // SAFETY: signal zero performs a permission and existence check only.
            if unsafe { libc::kill(reported, 0) } != 0 {
                return;
            }

            std::thread::sleep(core::time::Duration::from_millis(10));
        }

        panic!("the child outlived the interrupt");
    }

    /// The inner half of the test above: contains a child, then interrupts itself.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs in.
    #[cfg(unix)]
    #[test]
    fn spawns_a_child_then_interrupts_itself() {
        if std::env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let guard = contain(&mut command, Request::default()).expect("containment");

        // Deliberately never waited on: this process is about to be killed by the signal it is
        // here to raise, and the point of the test is what happens to the child when it is.
        #[expect(clippy::zombie_processes, reason = "this process does not outlive the child")]
        let child = command.spawn().expect("spawn");
        let subtree = Subtree::adopt(&child, guard).expect("adoption");

        println!("grandchild {}", child.id());

        // Kept alive so the slot stays claimed, which is exactly the state a run is in when the
        // interrupt arrives.
        core::mem::forget(subtree);

        // SAFETY: raising a signal at this process touches no memory.
        let _raised = unsafe { libc::raise(libc::SIGINT) };

        std::thread::sleep(core::time::Duration::from_secs(30));
    }
}
