//! Preparing, adopting, observing, and terminating one spawned process tree.

use core::time::Duration;
use std::io;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus};

use crate::{MemoryRequest, MemoryUsage};

#[cfg(target_os = "linux")]
use cargo_gamma_unsafe::cgroup::Cgroup;
#[cfg(unix)]
use cargo_gamma_unsafe::interrupt;
#[cfg(windows)]
use cargo_gamma_unsafe::job::{self, Job};

#[cfg(any(test, feature = "fault-injection"))]
use crate::faults;

/// How many concurrent child subtrees can be watched for terminal interruption.
#[cfg(unix)]
#[must_use]
pub const fn capacity() -> usize {
    interrupt::capacity()
}

/// Platforms without a shared interrupt registry have no process-wide watch limit.
#[cfg(not(unix))]
#[must_use]
pub const fn capacity() -> usize {
    usize::MAX
}

/// The resource control installed for one spawn, handed to [`ProcessTree::adopt`] afterwards.
///
/// Created before the child exists because every part of it has to be: a cgroup's `cgroup.procs`
/// must be open before the fork for the child to move itself, a job object's limits must be
/// configured before a process is assigned to it, and on Unix the interrupt handler has to have
/// been told that a group is about to exist before there is one. A job also implies a second thing
/// on Windows — the command is marked to start suspended, so that [`ProcessTree::adopt`] can assign the
/// child to the job and only then let it run.
///
/// Surrendered to [`ProcessTree::adopt`] by value, which is what bounds the Unix spawn window: the
/// window closes when this is dropped, and it cannot be dropped while the caller still owes the
/// adoption.
#[cfg_attr(not(unix), derive(Default))]
#[derive(Debug)]
pub struct SpawnGuard {
    /// The interrupt handler's promise not to finish this process until the child is watched.
    ///
    /// Held across the spawn. A signal arriving in that window would otherwise kill this process and
    /// leave the child running in a group nothing had been told about; see
    /// [`cargo_gamma_unsafe::interrupt`] for the protocol that closes it.
    #[cfg(unix)]
    spawning: interrupt::Spawning,

    /// The cgroup leaf the child will place itself in, on the platform that has them.
    #[cfg(target_os = "linux")]
    cgroup: Option<Cgroup>,

    /// The job the child will be assigned to, on the platform that has them.
    ///
    /// Its presence is also what says the child was asked to start suspended, and therefore has to
    /// be resumed once it is inside.
    #[cfg(windows)]
    job: Option<Job>,
}

impl SpawnGuard {
    /// Closes the interrupt window while a transient spawn refusal backs off, then reopens it.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "the shared API is fallible on Unix, where reopening the interrupt window can fail"
        )
    )]
    pub fn backoff(self, duration: Duration) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            let Self { spawning, cgroup } = self;

            drop(spawning);
            std::thread::sleep(duration);

            Ok(Self {
                spawning: window()?,
                cgroup,
            })
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let Self { spawning } = self;

            drop(spawning);
            std::thread::sleep(duration);

            Ok(Self { spawning: window()? })
        }

        #[cfg(not(unix))]
        {
            std::thread::sleep(duration);

            Ok(self)
        }
    }
}

/// Arranges for a child's descendants to be killable along with it, and accounted for.
///
/// Called before spawning, because on Unix the containment has to be requested as part of the
/// spawn itself, and because the interrupt window this opens is only worth anything if it is open
/// before the child exists.
///
/// # Errors
///
/// Returns the reason when `request` asked for measurement or a ceiling and the platform could not
/// install one. A run that asked to be protected and silently was not is the failure this reports
/// rather than swallows: the user would believe the machine was bounded, and find out otherwise
/// only when it was not.
///
/// Also returns a reason when an interrupt has already begun taking the run apart, since a process
/// free to die at the next instruction must not create a child that would outlive it.
#[cfg_attr(
    not(any(unix, windows)),
    expect(
        clippy::needless_pass_by_ref_mut,
        reason = "only the cgroup and job paths reach into the command, and the signature is shared"
    )
)]
pub fn prepare(command: &mut Command, request: MemoryRequest) -> Result<SpawnGuard, String> {
    #[cfg(any(test, feature = "fault-injection"))]
    if faults::fired(faults::Fault::Prepare) {
        return Err("the containment a test asked to fail could not be installed".to_owned());
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        interrupt::arm().map_err(|cause| format!("terminal-signal protection could not be installed: {cause}"))?;

        // The child leads its own process group, so a later signal to the negated group id reaches
        // every descendant that has not deliberately left the group. Without this the child shares
        // this process's group, and signalling the group would kill the run itself.
        let _ = command.process_group(0);
    }

    #[cfg(target_os = "linux")]
    {
        let cgroup = if request.wanted() {
            #[cfg(any(test, feature = "fault-injection"))]
            if faults::fired(faults::Fault::Boundary) {
                return Err("the accounting boundary a test asked to fail could not be created".to_owned());
            }

            let cgroup = Cgroup::create(request.limit)?;

            cgroup.arm(command)?;

            Some(cgroup)
        } else {
            None
        };

        // Opened last, once every fallible piece of setup is behind us. An open window defers the
        // whole run's response to `Ctrl-C`, so it is worth no more than it has to be: creating a
        // cgroup is several filesystem writes and, on the first contained spawn, can be a process
        // migration, none of which can produce a child. Nothing exists to protect until `spawn`.
        let spawning = window()?;

        Ok(SpawnGuard { spawning, cgroup })
    }

    #[cfg(windows)]
    {
        match Job::create(request.limit) {
            Some(job) => {
                // The child starts suspended so that it is inside the job before it executes an
                // instruction. Assigning an already-running process would leave it a window in
                // which it is bounded by nothing, and a test that allocates immediately — the one
                // shape a ceiling exists for — would spend that window doing exactly that.
                job::start_suspended(command);

                Ok(SpawnGuard { job: Some(job) })
            }
            // A job that could not be created has always meant a subtree this run cannot kill,
            // which it tolerates because killing one process is better than killing none. It
            // cannot tolerate it once the job is also the memory boundary.
            None if request.wanted() => {
                Err("a Windows job object could not be created, so this test binary's memory could not be accounted for".to_owned())
            }
            None => Ok(SpawnGuard { job: None }),
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
            // `support` cannot say yes on this platform, but its signature is shared with the two
            // that can, so the success case still needs an answer. Saying it plainly beats
            // `unwrap_or_else`, which reads as if it handled the error and in fact discards the
            // reason and yields `()` — this line did not compile at all until a macOS build was
            // first attempted.
            let Err(reason) = crate::support() else {
                return Err("this platform cannot bound a test subtree's memory".to_owned());
            };

            return Err(reason);
        }

        #[cfg(unix)]
        {
            Ok(SpawnGuard { spawning: window()? })
        }

        #[cfg(not(unix))]
        {
            Ok(SpawnGuard::default())
        }
    }
}

/// Opens the window in which a child may exist that no interrupt can yet reach.
///
/// Taken as late as possible — after every fallible piece of containment setup and immediately
/// before the caller spawns — because a handler that finds it open kills what it can see and then
/// declines to die, so the run absorbs `Ctrl-C` for as long as any window is held. The check is the
/// spawner's half of the handshake: a caller told the run is already being interrupted must create
/// nothing, since the process is free to die at the next instruction.
#[cfg(unix)]
fn window() -> Result<interrupt::Spawning, String> {
    #[cfg(any(test, feature = "fault-injection"))]
    if faults::fired(faults::Fault::Window) {
        return Err("the run is being interrupted, so nothing further was started".to_owned());
    }

    let spawning = interrupt::spawning();

    if spawning.interrupted() {
        return Err("the run is being interrupted, so nothing further was started".to_owned());
    }

    Ok(spawning)
}

/// A handle on a spawned child's whole subtree.
///
/// Created from an already-spawned child, and used once when the run decides the mutant has hung.
#[derive(Debug)]
pub struct ProcessTree {
    /// The leader retained until this subtree is observed or terminated.
    child: Option<Child>,

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
    cgroup: Option<Cgroup>,

    /// The job the child was placed in, on the platform that has them.
    #[cfg(windows)]
    job: Option<Job>,
}

impl ProcessTree {
    /// Takes hold of a freshly spawned child, together with whatever [`prepare`] set up for it.
    ///
    /// On Windows this is also where the child is let go: it was created suspended so that it
    /// could be placed inside its job before running, and it stays that way until this puts it
    /// there.
    ///
    /// # Errors
    ///
    /// Returns the reason when the guard could not be applied to the child that was just spawned.
    /// This method ends and reaps that child before returning: it is not safe to hand a caller a
    /// group leader it cannot watch, because a later cleanup would no longer be able to prove that
    /// its numeric group id still belongs to this run.
    #[cfg_attr(
        not(any(target_os = "linux", windows)),
        expect(
            clippy::needless_pass_by_value,
            reason = "the guard is surrendered here on every platform that has one, and taking it \
                      by value is what enforces that; here nothing is moved out of it, and the \
                      interrupt window it carries closes when it is dropped on the way out"
        )
    )]
    pub fn adopt(mut child: Child, guard: SpawnGuard) -> Result<Self, String> {
        #[cfg(any(test, feature = "fault-injection"))]
        if faults::fired(faults::Fault::Adopt) {
            abandon(&mut child, &guard);

            return Err("the accounting boundary a test asked to fail would not take the child".to_owned());
        }

        #[cfg(unix)]
        {
            let Ok(group) = i32::try_from(child.id()) else {
                abandon(&mut child, &guard);

                return Err("the child's process id does not fit a process group id, so it could not be watched for interrupts".to_owned());
            };

            // Registered so that an interrupt can reach it. A run cut off at the terminal takes its
            // children with it only while they share its group; since they lead their own, the
            // only thing that still knows about them is this list.
            //
            // The window `prepare` opened is still open here, which is what makes the registration
            // safe rather than merely quick: a handler that ran between the spawn and this line
            // could not have finished the process, and `watch` kills the group itself if one did
            // begin. The window closes when the guard is dropped at the end of this function.
            //
            // A registration that does not take is a refusal rather than a detail to store: the
            // child would run in a group of its own that no interrupt could reach, which is exactly
            // the leak this containment exists to close. `adopt` ends the child on this error,
            // so refusing here costs one mutant and leaks nothing.
            let Some(slot) = guard.spawning.watch(group) else {
                abandon(&mut child, &guard);

                return Err(format!(
                    "process group {group} could not be watched for interrupts, so the child would have outlived a cancelled run"
                ));
            };

            #[cfg(target_os = "linux")]
            {
                Ok(Self {
                    child: Some(child),
                    group: Some(group),
                    slot: Some(slot),
                    cgroup: guard.cgroup,
                })
            }

            #[cfg(not(target_os = "linux"))]
            {
                Ok(Self {
                    child: Some(child),
                    group: Some(group),
                    slot: Some(slot),
                })
            }
        }

        #[cfg(windows)]
        {
            if let Some(job) = guard.job.as_ref() {
                if !job.assign(&child) {
                    abandon(&mut child, &guard);

                    return Err("a Windows job object could not be given the test binary it was created for".to_owned());
                }

                // The child has been waiting inside the job since it was created. Nothing it does
                // from here can escape the ceiling, and nothing it did before now could have
                // reached it, because it has not run yet.
                if !job::release(child.id()) {
                    abandon(&mut child, &guard);

                    return Err("a Windows test binary could not be resumed inside the job object holding it".to_owned());
                }
            }

            Ok(Self {
                child: Some(child),
                job: guard.job,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let SpawnGuard {} = guard;

            Ok(Self { child: Some(child) })
        }
    }

    /// Takes the child's stdout pipe before observation starts.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    /// Takes the child's stderr pipe before observation starts.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    /// What the platform accounted for, read once the subtree has finished.
    #[must_use]
    #[cfg_attr(
        not(any(target_os = "linux", windows)),
        expect(
            clippy::unused_self,
            reason = "only the cgroup and job paths have anything to read, and the signature is shared"
        )
    )]
    pub fn usage(&self) -> MemoryUsage {
        #[cfg(target_os = "linux")]
        {
            self.cgroup.as_ref().map_or_else(MemoryUsage::default, |cgroup| {
                let (peak, exhausted) = cgroup.usage();

                MemoryUsage { peak, exhausted }
            })
        }

        #[cfg(windows)]
        {
            self.job.as_ref().map_or_else(MemoryUsage::default, |job| {
                let (peak, exhausted) = job.usage();

                MemoryUsage { peak, exhausted }
            })
        }

        #[cfg(not(any(target_os = "linux", windows)))]
        {
            MemoryUsage::default()
        }
    }

    /// Stops an interrupt from reaching this subtree.
    ///
    /// This is always called before reaping or draining output. A watch slot holds a numeric
    /// process-group id, and leaving it active after the leader is reaped could make a later
    /// interrupt signal a reused id.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_self,
            clippy::needless_pass_by_ref_mut,
            clippy::missing_const_for_fn,
            reason = "only the Unix watch list has a slot to hand back, and the signature is shared"
        )
    )]
    fn release(&mut self) {
        #[cfg(unix)]
        if let Some((slot, group)) = self.slot.take().zip(self.group) {
            // Named as well as numbered, so that a slot an interrupt already emptied and some
            // later spawn has since claimed is left to its new owner rather than cleared.
            interrupt::forget(slot, group);
        }
    }

    /// Observes a completed child, kills survivors, and only then reaps its leader.
    ///
    /// Unix uses `waitid(WNOWAIT)` to observe the exit without freeing the leader's pid or group
    /// id. That keeps `killpg` tied to this subtree even if this thread is preempted. The actual
    /// reap is deliberately internal to this lifecycle operation, so callers cannot restore a
    /// post-reap cleanup window by accident.
    pub fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| io::Error::other("the subtree leader was already reaped"))?;

        #[cfg(unix)]
        {
            let observed = match cargo_gamma_unsafe::group::exited(child.id()) {
                Ok(observed) => observed,
                Err(cause) => {
                    self.child = Some(child);

                    return Err(cause);
                }
            };

            let status = cleanup_after_observation(
                observed,
                || {
                    self.sweep();
                    self.release();
                },
                || child.wait(),
            )?;

            if status.is_none() {
                self.child = Some(child);
            }

            Ok(status)
        }

        #[cfg(not(unix))]
        {
            let status = match child.try_wait() {
                Ok(status) => status,
                Err(cause) => {
                    self.child = Some(child);

                    return Err(cause);
                }
            };

            if status.is_some() {
                // Windows jobs retain object handles rather than numeric process identifiers, and
                // platforms without groups have no identifier that a sweep could reuse.
                self.sweep();
                self.release();
            } else {
                self.child = Some(child);
            }

            Ok(status)
        }
    }

    /// Whether an interrupt can no longer reach this subtree through the watch list.
    ///
    /// Lets callers assert that release precedes output draining. The ordering is the one thing
    /// both reap paths guarantee, and it is the kind of ordering a later edit reverses without
    /// noticing.
    #[must_use]
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_self,
            reason = "only the Unix watch list has a registration to give up, and the signature is shared"
        )
    )]
    pub const fn released(&self) -> bool {
        #[cfg(unix)]
        {
            self.slot.is_none()
        }

        #[cfg(not(unix))]
        {
            true
        }
    }

    /// Kills the child and every process descended from it.
    ///
    /// Falls back to killing the child alone whenever the subtree cannot be reached, because a run
    /// that cut off one process is still better than one that cut off none.
    fn kill(&self, child: &mut Child) {
        self.sweep();

        // The group or job may not have covered the child — the id could not be converted, the job
        // could not be created — and in any case this is what makes `wait` return.
        let _killed = child.kill();
    }

    /// Ends the subtree and reaps its leader without exposing its group id to reuse.
    pub fn terminate(&mut self) -> io::Result<ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| io::Error::other("the subtree leader was already reaped"))?;

        self.kill(&mut child);
        self.release();

        child.wait()
    }

    /// Ends descendants while their leader's process-group id is still reserved.
    ///
    /// An exited leader can leave servers and inherited pipe handles behind. This private
    /// primitive is reachable only from [`Self::observe`] and [`Self::terminate`], which signal
    /// before reaping that leader, so `killpg` cannot name a replacement group.
    fn sweep(&self) {
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
            let _killed = cargo_gamma_unsafe::group::kill(group);
        }

        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }
    }

    #[cfg(all(test, unix))]
    fn id(&self) -> u32 {
        self.child.as_ref().expect("the subtree leader is live").id()
    }

    #[cfg(all(test, windows))]
    const fn child(&self) -> &Child {
        self.child.as_ref().expect("the subtree leader is live")
    }
}

/// Ends a child that failed containment before it can escape its unregistered subtree.
#[cfg_attr(
    not(any(target_os = "linux", windows)),
    expect(
        unused_variables,
        reason = "only Linux cgroups and Windows jobs carry containment state needed during abandonment"
    )
)]
fn abandon(child: &mut Child, guard: &SpawnGuard) {
    #[cfg(target_os = "linux")]
    if let Some(cgroup) = guard.cgroup.as_ref() {
        cgroup.kill();
    }

    #[cfg(unix)]
    if let Ok(group) = i32::try_from(child.id()) {
        let _killed = cargo_gamma_unsafe::group::kill(group);
    }

    #[cfg(windows)]
    if let Some(job) = guard.job.as_ref() {
        job.terminate();
    }

    let _killed = child.kill();
    let _reaped = child.wait();
}

/// Performs the only safe order after an exit observation.
///
/// Kept separate so the regression can run the exact order against a fake process-group backend
/// that reuses the group's numeric identifier as soon as its leader is reaped.
#[cfg(any(unix, test))]
fn cleanup_after_observation<T>(observed: bool, cleanup: impl FnOnce(), reap: impl FnOnce() -> io::Result<T>) -> io::Result<Option<T>> {
    if !observed {
        return Ok(None);
    }

    cleanup();
    reap().map(Some)
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.kill(&mut child);
            self.release();
            let _reaped = child.wait();
        } else {
            self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;
    use core::cell::RefCell;
    #[cfg(unix)]
    use std::process::Stdio;
    use std::thread;

    use crate::testing;

    use super::*;

    fn wait_for(subtree: &mut ProcessTree) -> ExitStatus {
        loop {
            match subtree.observe().expect("observe") {
                Some(status) => return status,
                None => thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// Reaping this fake leader instantly reuses its numeric group id for an unrelated process.
    ///
    /// It makes the old race deterministic: a cleanup that reaped before signalling would set
    /// `replacement_signalled`, rather than requiring PID-space wraparound at just the wrong
    /// scheduler instant to expose the fault.
    #[test]
    fn an_observed_exit_sweeps_before_a_reused_group_can_be_signalled() {
        #[derive(Default)]
        struct FakeGroup {
            reaped: bool,
            original_signalled: bool,
            replacement_signalled: bool,
        }

        impl FakeGroup {
            fn sweep(&mut self) {
                if self.reaped {
                    self.replacement_signalled = true;
                } else {
                    self.original_signalled = true;
                }
            }

            fn reap(&mut self) {
                self.reaped = true;
            }
        }

        let group = RefCell::new(FakeGroup::default());
        let result = cleanup_after_observation(
            true,
            || group.borrow_mut().sweep(),
            || {
                group.borrow_mut().reap();
                Ok(())
            },
        );

        assert!(matches!(result, Ok(Some(()))));

        let group = group.into_inner();

        assert!(group.original_signalled, "the original subtree was not swept");
        assert!(
            !group.replacement_signalled,
            "cleanup signalled the replacement group after the leader's id was reused"
        );
    }

    /// A request that asks for nothing gets containment without an accounting boundary.
    #[test]
    fn a_run_that_asks_for_no_accounting_reports_no_usage() {
        // Metering is opt-in, and a run that did not ask for it must not be told a peak of zero as
        // though it were a measurement.
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "true" });

        if cfg!(windows) {
            let _ = command.args(["/c", "exit 0"]);
        }

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");
        let child = command.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(child, guard).expect("adoption");

        let _status = wait_for(&mut subtree);

        assert_eq!(subtree.usage(), MemoryUsage::default());
    }

    #[test]
    fn a_request_that_asks_for_nothing_is_recognized_as_such() {
        assert!(!MemoryRequest::default().wanted());
        assert!(MemoryRequest { meter: true, limit: None }.wanted());
        assert!(
            MemoryRequest {
                meter: false,
                limit: Some(1)
            }
            .wanted()
        );
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
        match prepare(&mut command, MemoryRequest { meter: true, limit: None }) {
            Ok(_guard) => crate::support().expect("metering succeeded, so it is supported"),
            Err(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
        }
    }

    #[test]
    fn a_contained_child_can_still_be_run_and_waited_for() {
        // Containment must not change what a normal run does, only what a kill reaches. On Windows
        // this is also what says the child was let out of the suspension it is created in: one
        // that was never resumed would hang here rather than exit.
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "true" });

        if cfg!(windows) {
            let _ = command.args(["/c", "exit 0"]);
        }

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");

        let child = command.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(child, guard).expect("adoption");

        assert!(wait_for(&mut subtree).success());
    }

    /// A metered child's peak is reported through the subtree, on a host that can measure one.
    #[test]
    fn a_metered_subtree_reports_what_it_used() {
        if testing::without_memory_support("a metered subtree reporting its peak") {
            return;
        }

        // The compiled helper rather than a shell pipeline, so that this runs on every platform
        // the tool builds for. Containment and accounting are promises about somebody else's
        // machine, and a fixture that only exists on Unix leaves the whole of the job-object
        // implementation unproven.
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("eat:32"), testing::directive("exit:0")]);

        let guard = prepare(&mut command, MemoryRequest { meter: true, limit: None }).expect("containment");
        let child = command.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(child, guard).expect("adoption");

        let _status = wait_for(&mut subtree);
        let usage = subtree.usage();

        // The measurement is what every derived ceiling is built from, so a boundary that
        // installed itself and then measured nothing would be worse than none at all.
        assert!(usage.peak.is_some(), "{usage:?}");
    }

    /// A subtree that never got a process group still kills the child directly, rather than
    /// silently doing nothing.
    ///
    /// The conversion from a process id to a group only fails when the id does not fit a signal's
    /// idea of one, which cannot happen on a real Linux host; but `kill` cannot assume the
    /// conversion always succeeded, and this is the fallback that keeps a wrong assumption from
    /// turning into a run that believed it had cut a mutant off while the process kept running.
    #[cfg(unix)]
    #[test]
    fn a_subtree_with_no_group_still_kills_the_child_directly() {
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "sleep 30"]);
        let mut child = command.spawn().expect("spawn");

        let subtree = ProcessTree {
            child: None,
            group: None,
            slot: None,
            #[cfg(target_os = "linux")]
            cgroup: None,
        };

        subtree.kill(&mut child);

        let status = child.wait().expect("wait");

        // With no group to signal, the fallback still has to reach the child itself, or a subtree
        // this run could not fully identify would simply be left running.
        assert!(!status.success(), "{status:?}");
    }

    /// A subtree with no job still kills the child directly.
    ///
    /// The Windows counterpart of the fallback above. A job object can fail to be created — an
    /// existing job on the process that forbids nesting, a policy that refuses one — and
    /// containment tolerates that, because killing one process is better than killing none. What
    /// it must not do is decide it has nothing to kill.
    #[cfg(windows)]
    #[test]
    fn a_subtree_with_no_job_still_kills_the_child_directly() {
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("sleep:30000"), testing::directive("exit:0")]);

        let mut child = command.spawn().expect("spawn");
        let subtree = ProcessTree { child: None, job: None };

        subtree.kill(&mut child);

        let status = child.wait().expect("wait");

        assert!(!status.success(), "{status:?}");
    }

    /// A contained child is inside a job object, which is what a later kill reaches through.
    ///
    /// The Windows counterpart of `a_contained_child_leads_its_own_process_group`. Both say the
    /// same thing in the platform's own terms: the descendants can be reached without reaching
    /// this run. It also says the child was let out of the suspension it is created in, since a
    /// process still suspended would never reach the assignment being asserted.
    #[cfg(windows)]
    #[test]
    fn a_contained_child_is_placed_inside_a_job() {
        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.args([testing::directive("sleep:30000"), testing::directive("exit:0")]);

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");
        let child = command.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(child, guard).expect("adoption");
        let inside = job::in_any_job(subtree.child());

        let _reaped = subtree.terminate();

        assert_eq!(inside, Some(true), "the contained child was left outside every job");
    }

    /// A subtree that never claimed a slot in the interrupt registry drops without touching it.
    ///
    /// `forget` releases whatever an interrupt would otherwise signal, and a slot of `None` means
    /// this subtree was never registered in the first place — calling it anyway would release
    /// whatever unrelated child had since taken slot zero, and an interrupt would then miss the
    /// group it was meant to kill and take a stranger's instead.
    #[cfg(unix)]
    #[test]
    fn a_subtree_with_no_watched_slot_drops_without_touching_the_registry() {
        let subtree = ProcessTree {
            child: None,
            group: None,
            slot: None,
            #[cfg(target_os = "linux")]
            cgroup: None,
        };

        drop(subtree);
    }

    /// Releasing a subtree frees its slot there and then, rather than at the drop.
    ///
    /// Every path that reaps a child releases before it drains that child's output, and the drain
    /// is the long part: a slot still naming a reaped pid is a slot naming whatever took the id
    /// next, and the interrupt handler kills what the slot names.
    #[cfg(unix)]
    #[test]
    fn releasing_a_subtree_frees_its_slot_immediately_rather_than_at_drop() {
        let group = 0x0060_0d1e;
        let spawning = interrupt::spawning();
        let slot = spawning.watch(group).expect("a free slot");

        let mut subtree = ProcessTree {
            child: None,
            group: Some(group),
            slot: Some(slot),
            #[cfg(target_os = "linux")]
            cgroup: None,
        };

        assert_eq!(interrupt::watched(slot), group, "the slot names this subtree's group");
        assert!(!subtree.released(), "a subtree still holding a slot is not released");

        subtree.release();

        assert_eq!(
            interrupt::watched(slot),
            0,
            "releasing must free the slot without waiting for the drop"
        );
        assert!(subtree.released(), "and must say so, since the drain ordering is asserted on it");

        // Whatever claims the slot next belongs to a different child, so neither a second release
        // nor the drop that follows may take it away from them.
        let taken = spawning.watch(0x0060_0d1f).expect("a free slot");

        subtree.release();
        drop(subtree);

        assert_eq!(interrupt::watched(taken), 0x0060_0d1f, "a later child's slot must survive");

        interrupt::forget(taken, 0x0060_0d1f);
    }

    /// Forgetting a slot past the end of the registry does nothing, rather than panicking.
    ///
    /// The slot a `ProcessTree` carries always came from `watch`, which never hands out an
    /// out-of-range index; but `forget` runs from a `Drop` implementation, where a panic would abort
    /// whatever mutant run was unwinding through it instead of merely leaving one interrupt
    /// registration stale.
    #[cfg(unix)]
    #[test]
    fn forgetting_an_out_of_range_slot_does_nothing() {
        interrupt::forget(usize::MAX, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_contained_child_leads_its_own_process_group() {
        // Which is what makes the group signal reach the descendants without reaching this run.
        let mut command = Command::new("sh");
        let _ = command.args(["-c", "sleep 30"]);

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");

        let child = command.spawn().expect("spawn");
        let id = i32::try_from(child.id()).expect("pid");

        assert_eq!(cargo_gamma_unsafe::group::group_of(id), Some(id));

        let mut subtree = ProcessTree::adopt(child, guard).expect("adoption");
        let _reaped = subtree.terminate();
    }

    /// Killing a subtree stops a grandchild that outlived its parent, on every platform.
    ///
    /// The whole point of containment: a test that spawned something is not cut off by killing the
    /// harness. The grandchild deliberately outlives its parent, which is the shape that leaves an
    /// orphan holding the scratch tree and the caller's pipe.
    ///
    /// Written against files rather than against pids, so that it says the same thing on Unix,
    /// where a survivor would be found with signal zero, and on Windows, where the equivalent is a
    /// handle query. It is also the stronger claim of the two: a killed process that had already
    /// done its work is not contained in any sense that matters, and a liveness check would not
    /// notice.
    #[test]
    fn killing_the_subtree_reaches_a_grandchild() {
        let work = testing::workdir("gamma-grandchild");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, finished) = (base.join("started"), base.join("finished"));

        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command.arg(testing::directive(format_args!(
            "spawn:touch:{started}|sleep:2000|touch:{finished}"
        )));
        let _ = command.arg(testing::directive("sleep:30000"));

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");
        let child = command.spawn().expect("spawn");
        let mut subtree = ProcessTree::adopt(child, guard).expect("adoption");

        // Waited for rather than assumed: killing before the grandchild exists would pass without
        // containment reaching anything at all.
        let mut running = false;

        for _attempt in 0..600 {
            if started.exists() {
                running = true;
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(running, "the grandchild never started, so the kill proves nothing");

        let _reaped = subtree.terminate();

        // Past the grandchild's own sleep with margin, so an unreached one has had every chance to
        // finish its work and say so.
        thread::sleep(Duration::from_secs(4));

        assert!(!finished.exists(), "the grandchild kept working after the subtree was killed");
    }

    #[test]
    fn dropping_an_unreleased_subtree_reaps_its_leader_and_grandchild() {
        let work = testing::workdir("gamma-drop-grandchild");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, finished) = (base.join("started"), base.join("finished"));

        let mut command = Command::new(testing::helper_binary_path().as_std_path());
        let _ = command
            .arg(testing::directive(format_args!(
                "spawn:touch:{started}|sleep:2000|touch:{finished}"
            )))
            .arg(testing::directive("sleep:30000"));

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");
        let child = command.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(child, guard).expect("adoption");

        for _attempt in 0..600 {
            if started.exists() {
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(started.exists(), "the grandchild never started");
        drop(subtree);
        thread::sleep(Duration::from_secs(3));
        assert!(!finished.exists(), "the dropped subtree left its grandchild alive");
    }

    /// The fixture the test above rests on really does outlive its parent and really does finish.
    ///
    /// Without this, a helper whose `spawn` quietly did nothing would make that test pass by
    /// producing no grandchild to reach — the exact shape of a test that cannot fail.
    #[test]
    fn an_unkilled_grandchild_outlives_its_parent_and_finishes() {
        let work = testing::workdir("gamma-grandchild-control");
        let base = Utf8Path::from_path(work.path()).expect("the temporary path is UTF-8");
        let (started, finished) = (base.join("started"), base.join("finished"));

        let began = std::time::Instant::now();
        let mut child = Command::new(testing::helper_binary_path().as_std_path())
            .arg(testing::directive(format_args!(
                "spawn:touch:{started}|sleep:2000|touch:{finished}"
            )))
            .arg(testing::directive("exit:0"))
            .spawn()
            .expect("spawn");

        assert!(child.wait().expect("wait").success());

        // The parent is gone well before the grandchild's work is done, which is what makes the
        // grandchild an orphan rather than a child, and this test a fixture check rather than a
        // restatement of "a process this run waited for had finished".
        let waited = began.elapsed();

        assert!(
            waited < Duration::from_millis(1500),
            "the parent outlived the grandchild: {waited:?}"
        );
        assert!(!finished.exists(), "the grandchild finished before its parent did");

        for _attempt in 0..600 {
            if finished.exists() {
                return;
            }

            thread::sleep(Duration::from_millis(10));
        }

        panic!("the orphaned grandchild never finished its work");
    }

    /// Runs one of the self-interrupting inner tests below, and reports the pid it spawned.
    ///
    /// In a subprocess, because the thing under test ends by re-raising a terminal signal and
    /// killing whoever is running it.
    #[cfg(unix)]
    fn interrupted_run(inner: &str) -> i32 {
        let program = std::env::args().next().expect("this test binary");
        let mut child = Command::new(&program)
            .args(["--exact", "--nocapture", &format!("process_tree::tests::{inner}")])
            .env("GAMMA_INTERRUPT_CHILD", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");

        // Read line by line and only as far as the marker, rather than to end of file: the child
        // the inner test spawns inherits this pipe, so reading it to the end would wait out that
        // child — and this test is about to ask whether the interrupt cut it short, which is a
        // question already answered yes by anything that waited for it first.
        //
        // The marker is looked for anywhere in a line rather than as a line of its own, because the
        // harness running the inner test writes its own progress on the same line when it is
        // running single-threaded, which is what an inherited `RUST_TEST_THREADS` makes it do.
        let reported = {
            use std::io::BufRead as _;

            let said = child.stdout.take().expect("pipe");

            io::BufReader::new(said)
                .lines()
                .find_map(|line| {
                    let text = line.ok()?;
                    let (_before, rest) = text.split_once("grandchild ")?;

                    rest.split_whitespace().next()?.parse().ok()
                })
                .expect("the inner test reports the pid it spawned")
        };

        // The inner process dies of the signal it raised, one way or another; this is only here so
        // it is reaped rather than left as a zombie for the rest of the suite.
        let _status = child.wait();

        reported
    }

    /// Whether a process has gone within a couple of seconds, which is what a kill looks like.
    #[cfg(unix)]
    fn gone_soon(pid: i32) -> bool {
        for _attempt in 0..200 {
            if !cargo_gamma_unsafe::group::exists(pid) {
                return true;
            }

            thread::sleep(Duration::from_millis(10));
        }

        false
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_takes_the_children_with_it() {
        // A child in its own process group no longer dies with the terminal's `Ctrl-C`, so the run
        // has to take it along deliberately.
        testing::within(testing::WATCHDOG, "an interrupt with a child watched", || {
            let reported = interrupted_run("spawns_a_child_then_interrupts_itself");

            assert!(gone_soon(reported), "the child outlived the interrupt");
        });
    }

    /// A signal arriving between the spawn and the registration still takes the child.
    ///
    /// The window that would lose it: the child exists, leads a process group of its own, and
    /// nothing has been told about that group, so a handler scanning the registry finds nothing to
    /// kill and re-raises — the run dies of the signal and the child, which the terminal's signal
    /// never reached either, goes on running. Delivered on the spawning thread itself, which is
    /// both the deterministic way to land in the window and the case a handler cannot wait its way
    /// out of.
    #[cfg(unix)]
    #[test]
    fn an_interrupt_inside_the_spawn_window_still_takes_the_child() {
        testing::within(testing::WATCHDOG, "an interrupt inside the spawn window", || {
            let reported = interrupted_run("interrupts_itself_between_the_spawn_and_the_registration");

            assert!(gone_soon(reported), "the child created inside the spawn window survived");
        });
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

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");

        // Deliberately never waited on: this process is about to be killed by the signal it is
        // here to raise, and the point of the test is what happens to the child when it is.
        let child = command.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(child, guard).expect("adoption");

        println!("grandchild {}", subtree.id());

        // Kept alive so the slot stays claimed, which is exactly the state a run is in when the
        // interrupt arrives.
        core::mem::forget(subtree);

        cargo_gamma_unsafe::group::raise_interrupt();

        thread::sleep(Duration::from_secs(30));
    }

    /// The inner half of the window test: interrupts itself with the child spawned and unwatched.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs
    /// in. The signal is raised on this thread, so the handler runs here — the one case a handler
    /// could not have waited its way out of, because the thread it would be waiting for is the one
    /// it is running on.
    #[cfg(unix)]
    #[test]
    fn interrupts_itself_between_the_spawn_and_the_registration() {
        if std::env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");

        let child = command.spawn().expect("spawn");

        // Said before the signal, because nothing after the adoption below runs: the guard's window
        // closes inside it, and closing the last window of an interrupted run ends this process.
        println!("grandchild {}", child.id());

        cargo_gamma_unsafe::group::raise_interrupt();

        // Reached only because the handler deferred rather than re-raising, which is the protocol
        // this test exists for. Registering the group is also what kills it.
        let subtree = ProcessTree::adopt(child, guard).expect("adoption");

        core::mem::forget(subtree);

        thread::sleep(Duration::from_secs(30));
    }

    /// The terminal's *other* stop keystroke takes the children too.
    ///
    /// `Ctrl-\\` sends `SIGQUIT` to the whole foreground process group — but containment has
    /// already moved every child out of that group precisely so the terminal cannot reach it, so a
    /// signal this run does not handle kills the run alone and leaves the entire subtree alive
    /// holding its scratch trees. Handling `SIGINT` and not `SIGQUIT` is therefore not a smaller
    /// version of the same guarantee, it is the absence of it on one keystroke.
    #[cfg(unix)]
    #[test]
    fn a_quit_takes_the_children_with_it_too() {
        testing::within(testing::WATCHDOG, "a quit with a child watched", || {
            let reported = interrupted_run("spawns_a_child_then_quits_itself");

            assert!(gone_soon(reported), "the child outlived the quit");
        });
    }

    /// A second signal arriving during a deferred interrupt still ends with the child dead.
    ///
    /// The disposition has to survive its own delivery for this to hold. Where it does not — the
    /// System V reading of `signal()`, which POSIX permits — the first signal resets the handler to
    /// the default, and since that first handler *deferred* rather than dying, the second signal
    /// terminates the run outright: nothing sweeps the registry, and every contained child survives
    /// in a group nothing signalled. Inert on glibc, musl and the BSDs, which all give the other
    /// reading; the explicit `sigaction` flags are what make it true everywhere, and
    /// `every_terminal_signal_is_armed_to_survive_its_own_delivery` is what checks them.
    #[cfg(unix)]
    #[test]
    fn a_second_interrupt_during_a_deferred_one_still_takes_the_child() {
        testing::within(testing::WATCHDOG, "a second interrupt while deferred", || {
            let reported = interrupted_run("interrupts_itself_twice_with_a_window_open");

            assert!(gone_soon(reported), "the child survived the second interrupt");
        });
    }

    /// The inner half of the quit test: contains a child, then quits on itself.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs in.
    #[cfg(unix)]
    #[test]
    fn spawns_a_child_then_quits_itself() {
        if std::env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");

        let child = command.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(child, guard).expect("adoption");

        println!("grandchild {}", subtree.id());

        // Kept alive so the slot stays claimed, which is the state a run is in when the signal
        // arrives.
        core::mem::forget(subtree);

        cargo_gamma_unsafe::group::raise_quit();

        thread::sleep(Duration::from_secs(30));
    }

    /// The inner half of the two-signal test: defers one interrupt, then takes another.
    ///
    /// Inert unless the outer test asked for it, since it deliberately kills the process it runs
    /// in. The child is watched before either signal, and a second window is then held open so the
    /// handler defers instead of dying — which is the only state in which a reset disposition can
    /// be observed, because it is the only one where the run is still alive to receive a second
    /// signal. Closing that window is what finally performs the death, and the sweep with it.
    #[cfg(unix)]
    #[test]
    fn interrupts_itself_twice_with_a_window_open() {
        if std::env::var_os("GAMMA_INTERRUPT_CHILD").is_none() {
            return;
        }

        let mut command = Command::new("sleep");
        let _ = command.arg("60");

        let guard = prepare(&mut command, MemoryRequest::default()).expect("containment");

        let child = command.spawn().expect("spawn");
        let subtree = ProcessTree::adopt(child, guard).expect("adoption");

        // A second window, for a spawn that never happens: it is what makes the handler defer.
        let mut pending = Command::new("sleep");
        let _ = pending.arg("60");

        let held = prepare(&mut pending, MemoryRequest::default()).expect("containment");

        // Said before either signal, because the run ends inside the drop below.
        println!("grandchild {}", subtree.id());

        core::mem::forget(subtree);

        cargo_gamma_unsafe::group::raise_interrupt();

        // Reached only because the handler deferred. Under a disposition that reset on delivery,
        // this second raise is the default action and the child below is never swept.
        cargo_gamma_unsafe::group::raise_interrupt();

        // Closing the last window is what kills the watched group and then this process.
        drop(held);

        thread::sleep(Duration::from_secs(30));
    }

    /// The accounting boundary is built before the spawn window opens, not inside it.
    ///
    /// An open window is the run's whole response to `Ctrl-C` held back: `Registry::interrupt` sees
    /// a spawn in flight, declines to die, and defers to whoever closes the last window. That is
    /// exactly right for the interval it exists to cover — a child that exists but is not yet
    /// watched — and exactly wrong for anything else, because nothing bounds how long the other
    /// work takes. Creating a cgroup is a `mkdir` and several interface-file writes, and on the
    /// first contained spawn of a run it can be a whole controller discovery and a process
    /// migration; on a filesystem that stalls, holding the window across it turns `SIGINT`,
    /// `SIGTERM`, `SIGHUP` and `SIGQUIT` into signals the run absorbs until somebody reaches for
    /// `SIGKILL`. Nothing exists to protect until `spawn`, so the window is worth opening only
    /// after every fallible step before it has succeeded.
    ///
    /// Read from which failure comes back when both steps are asked to fail: the first one to run
    /// is the one that reports, so this pins the order rather than the timing, which is the part a
    /// future edit could quietly reverse.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_accounting_boundary_is_built_before_the_spawn_window_opens() {
        let boundary = faults::arm(faults::Fault::Boundary);
        let window = faults::arm(faults::Fault::Window);

        let mut command = Command::new("true");
        let reason = prepare(&mut command, MemoryRequest { meter: true, limit: None })
            .expect_err("both steps were asked to fail, so containment cannot have succeeded");

        assert!(
            reason.contains("accounting boundary"),
            "the spawn window was opened before the accounting boundary was built: {reason}"
        );

        drop((boundary, window));
    }
}
