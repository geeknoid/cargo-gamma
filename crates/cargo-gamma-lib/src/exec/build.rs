use camino::{Utf8Path, Utf8PathBuf};
use core::ops::Range;
use core::time::Duration;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::process::{ExitStatus, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::{HashMap, HashSet};
use crate::Result;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::model::{Mutant, Outcome};
use crate::schema::{self, Guard, Position};

use super::cargo_options::BuildLimits;
use super::test_binary::{TestBinary, test_binaries};
use super::verdict::tail;
use super::workspace::Workspace;

/// Where each live mutant's guard landed, by ordinal, paired with the file it landed in.
type Guards = HashMap<u32, (Utf8PathBuf, Guard)>;

/// What a build round produced.
#[derive(Debug, Default)]
pub(super) struct Build {
    pub(super) binaries: Vec<TestBinary>,
    pub(super) withdrawn: usize,
    pub(super) rounds: u32,

    /// Whether a narrowed build was abandoned and the whole workspace built instead.
    pub(super) widened: bool,
}

/// Drives the build, withdrawing mutants that cannot compile until what is asked for compiles.
///
/// A run converges the workspace one stage at a time and then once as a whole, and every one of
/// those builds shares the same withdrawal set, round counter and budget reference. Holding them
/// together is what lets a stage inherit what earlier stages already ruled out.
#[derive(Debug, Default)]
pub(super) struct Converger {
    withdrawn: HashSet<u32>,
    rounds: u32,

    /// How many mutants each failed round withdrew, oldest first.
    ///
    /// Kept so that a run which hits the limit can say whether it was converging, which is the only
    /// thing that decides whether raising the limit would have helped.
    per_round: Vec<usize>,

    /// How long the first build of the run took, which every later budget is scaled from.
    ///
    /// Set once and never reset. A stage builds a fraction of the workspace, so letting a small
    /// stage set this reference would leave every later stage — and the whole-workspace build that
    /// follows them — with a budget derived from a build that was never comparable.
    first_round: Option<Duration>,
}

impl Converger {
    /// Instruments the tree and builds it until it compiles, withdrawing whatever stands in the way.
    ///
    /// `select` names the packages to build, or is `None` for the whole workspace. `verb` is the
    /// cargo command and its flags.
    ///
    /// Returns cargo's JSON stream from the build that finally succeeded.
    fn converge(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
    ) -> Result<String> {
        loop {
            self.rounds = self.rounds.saturating_add(1);

            let guards = instrument_tree(work, plan, &self.withdrawn)?;

            let started = Instant::now();
            let outcome = run_cargo(work, plan, verb, select, limits, self.first_round)?;
            let elapsed = started.elapsed();

            let Some(stdout) = outcome.stdout else {
                let budget = limits.budget(self.first_round).unwrap_or(elapsed);

                return Err(Self::build_timeout_error(budget));
            };

            if self.first_round.is_none() {
                self.first_round = Some(elapsed);
            }

            if outcome.succeeded {
                return Ok(stdout);
            }

            let blamed = blame(&stdout, &work.root, &guards);

            if blamed.is_empty() {
                return Err(Self::unattributed_build_error(work, &stdout, &outcome.stderr));
            }

            if self.rounds >= limits.rounds() {
                return Err(Self::rollback_limit_error(self.rounds, &self.per_round, work, &stdout));
            }

            self.per_round.push(blamed.len());
            self.withdrawn.extend(blamed);
        }
    }

    fn build_timeout_error(budget: Duration) -> Error {
        error!(
            "the build was still running after {budget:.0?} and was stopped. A run builds once, so a \
             build that does not finish costs the whole run; raise --build-timeout if this one is simply slow."
        )
    }

    fn unattributed_build_error(work: &Workspace, stdout: &str, stderr: &str) -> Error {
        let diagnostics = diagnostics(stdout);

        // A build that produced no diagnostics at all did not fail the way this message assumes.
        // The compiler was never reached — a build script panicked, a native library is missing, a
        // dependency would not resolve, a package spec was ambiguous — and every one of those is
        // explained on stderr and nowhere else. Saying "does not compile" here would send the
        // reader hunting for a broken mutant that was never generated.
        if diagnostics.trim().is_empty() {
            return error!(
                "the instrumented tree failed to build, and the compiler reported nothing, so no \
                 mutant can be blamed for it. The cause is usually something cargo hit before it \
                 reached the code — a build script, a missing native dependency, a bad invocation — \
                 and it is almost always in what cargo said:\n\n{}\n\n{}",
                tail(&complaints(stderr), 30),
                work.inspect_hint()
            );
        }

        error!(
            "the instrumented tree does not compile and the failure could not be attributed to a mutant.\n\
             {}\n\n{}",
            work.inspect_hint(),
            tail(&diagnostics, 40)
        )
    }

    fn rollback_limit_error(rounds: u32, per_round: &[usize], work: &Workspace, stdout: &str) -> Error {
        let withdrawn: usize = per_round.iter().sum();

        // Whether the rounds were still making progress is the one thing that decides what to do
        // next, and it is invisible from a total. A falling tail means the cap was simply too low
        // for this tree; a flat one means each round is uncovering as much as the last, and more
        // rounds will not help.
        let recent: Vec<String> = per_round.iter().rev().take(5).rev().map(usize::to_string).collect();

        let advice = if per_round.last().copied().unwrap_or(0) == 0 {
            "The last round withdrew nothing, so the tree does not compile for a reason no mutant \
             explains; more rounds will not help."
        } else {
            "If those counts are falling, the tree was converging and --rollback-rounds is simply \
             too low for it. If they are flat, each round is uncovering as much as the last and \
             raising the limit will only make the failure slower."
        };

        error!(
            "the instrumented tree still does not compile after {rounds} rounds of withdrawing \
             unviable mutants ({withdrawn} withdrawn in total).\n\
             Withdrawals in the last rounds: {}.\n{advice}\n\
             {}\n\n{}",
            recent.join(", "),
            work.inspect_hint(),
            tail(&diagnostics(stdout), 40)
        )
    }

    fn missing_guard_error(missing: &Mutant) -> Error {
        error!(
            "internal error: no guard was emitted for the mutant at {}:{}, so it could not \
             be tested. Please report this.\n  {}",
            missing.file,
            missing.line,
            missing.describe()
        )
    }

    /// Compiles one stage's libraries, so its mutants are ruled on before anything downstream.
    ///
    /// Only libraries and binaries are built, never test targets. Every mutant lives in one of
    /// those, so this sees every diagnostic a mutant can cause, and it avoids the one thing a
    /// subset build cannot reproduce: cargo resolves features over the packages being built, so a
    /// test target that relies on a feature some other package switches on does not compile on its
    /// own. Those targets are left to the whole-workspace build, where the features are the real
    /// ones.
    pub(super) fn stage(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        packages: &[String],
        limits: BuildLimits,
    ) -> Result<()> {
        // Nothing is narrated from inside a stage: the stage reports what it found and what it
        // withdrew as one line when it is done, and a round-by-round commentary underneath that
        // would bury the sequence the whole arrangement exists to show.
        let _stdout = self.converge(work, plan, Some(packages), &["build", "--keep-going"], limits)?;

        Ok(())
    }

    /// Compiles the test targets of `select`, or of the whole workspace when it is `None`.
    ///
    /// Returns cargo's JSON stream from the `cargo test --no-run` half, which is the one whose
    /// artifact messages name the binaries.
    fn compile(&mut self, work: &Workspace, plan: &Plan, select: Option<&[String]>, limits: BuildLimits) -> Result<String> {
        // Convergence runs `cargo build --tests --keep-going` rather than `cargo test --no-run`,
        // so that a crate whose sibling failed is still compiled and still contributes its
        // diagnostics. `cargo test` does not accept the flag, hence the split below.
        let _stdout = self.converge(work, plan, select, &["build", "--tests", "--keep-going"], limits)?;

        // The binaries that get run have to come from the command that would run them, so that
        // nothing compiles under one set of flags and then fails to appear under the other. This
        // only happens once the tree is known to build, so it costs a cache hit rather than a
        // compile — and if it does fail, its diagnostics are blamed like any other round's.
        self.converge(work, plan, select, &["test", "--no-run"], limits)
    }

    /// Builds the whole workspace, which is what decides the run.
    ///
    /// The staged builds before this one are a way of ruling on mutants early and of saying what is
    /// happening while it happens; this is the build whose feature resolution matches the one
    /// `cargo test` would use, and the binaries come from it for that reason.
    pub(super) fn finish(
        mut self,
        work: &Workspace,
        plan: &mut Plan,
        select: Option<&[String]>,
        limits: BuildLimits,
    ) -> Result<Build> {
        let mut widened = false;

        let stdout = match self.compile(work, plan, select, limits) {
            Ok(stdout) => stdout,

            // A narrowed build is not merely a smaller version of the whole one: cargo unifies
            // features over the packages it is told to build, so a test target that only compiles
            // because a package left out of the selection switches a feature on will fail here and
            // will fail in a way no mutant can be blamed for. That is a wrong answer to the
            // question the run is asking, so the selection is abandoned rather than reported.
            Err(narrow) if select.is_some() => {
                widened = true;

                self.compile(work, plan, None, limits).map_err(|_whole| narrow)?
            }

            Err(error) => return Err(error),
        };

        for mutant in &mut plan.mutants {
            if self.withdrawn.contains(&mutant.ordinal) {
                mutant.outcome = Outcome::CompileError;
            }
        }

        // A mutant whose file the compiler never read cannot be judged by any test, so it is taken
        // out of the run here rather than left to be reported as a survivor later. This runs after
        // the withdrawal above so that a mutant which genuinely failed to compile keeps that more
        // specific verdict.
        //
        // The agreement check in front of it exists because the failure this could otherwise cause
        // is silent and expensive: if dep-info ever spelled its paths differently from the way the
        // survey spells them, nothing would match, every mutant would be excused, and the run would
        // report a flattering score with no sign that anything had gone wrong. A set that names not
        // one file the survey found is a set we do not understand, so nothing is concluded from it.
        if let Some(compiled) = compiled_sources(&stdout, &work.root)
            && plan.files.iter().any(|file| compiled.contains(&file.path))
        {
            for mutant in &mut plan.mutants {
                if mutant.outcome == Outcome::Pending && !compiled.contains(&mutant.file) {
                    mutant.outcome = Outcome::NotBuilt;
                }
            }
        }

        Ok(Build {
            binaries: test_binaries(&stdout),
            withdrawn: self.withdrawn.len(),
            rounds: self.rounds,
            widened,
        })
    }

    /// How many mutants have been withdrawn so far.
    pub(super) fn withdrawn(&self) -> usize {
        self.withdrawn.len()
    }
}

/// Names every source file the compiler actually read, according to cargo's dep-info.
///
/// Returns `None` when no dep-info could be read at all, which has to mean "do not draw any
/// conclusion" rather than "nothing was compiled": treating an unreadable scratch tree as an empty
/// set would condemn every mutant in the run as unbuilt.
///
/// Cargo writes a `.d` file beside each artifact listing the sources that went into it, in the
/// makefile format `target: dep dep dep`. That list is the compiler's own account of what it read,
/// which is the only thing that answers the question honestly. Evaluating `#[cfg]` predicates
/// ourselves would mean reimplementing feature resolution, target detection and every other
/// predicate cargo and rustc already agreed on, and being subtly wrong about it in the cases that
/// matter most.
///
/// Which `.d` files belong to *this* build is decided from the build's own artifact messages
/// rather than by looking at what is on disk. The scratch target directory is deliberately kept
/// between runs so that builds are incremental, so it accumulates dep-info from every earlier run
/// as well — including runs with a different feature set. Reading all of it would union today's
/// answer with a previous one and quietly conclude that everything was compiled, which is the
/// wrong answer in exactly the case this is here to catch.
fn compiled_sources(stdout: &str, root: &Utf8Path) -> Option<HashSet<Utf8PathBuf>> {
    let mut compiled: HashSet<Utf8PathBuf> = HashSet::default();
    let mut read_any = false;

    for dep_file in dep_files(stdout) {
        let Ok(text) = fs::read_to_string(dep_file.as_std_path()) else {
            continue;
        };

        read_any = true;

        for line in text.lines() {
            let Some((_artifact, dependencies)) = line.split_once(": ") else {
                continue;
            };

            for path in dependencies.split_whitespace() {
                let path = Utf8Path::new(path);
                let relative = path.strip_prefix(root).unwrap_or(path);

                let _added = compiled.insert(Utf8PathBuf::from(normalize_separators(relative.as_str())));
            }
        }
    }

    read_any.then_some(compiled)
}

/// Names the dep-info file for every unit in one build, from cargo's JSON artifact messages.
///
/// Cargo does not list the `.d` file among an artifact's `filenames`, but it does name it after
/// the same unit hash, so an artifact at `deps/libfoo-9a3f.rmeta` is described by `deps/foo-9a3f.d`.
/// Deriving the name from the hash rather than from the file stem avoids having to reproduce
/// cargo's own rules about which artifact kinds carry a `lib` prefix.
///
/// Uplifted copies such as `debug/libfoo.rlib` are skipped: they carry no hash, so their dep-info
/// is overwritten by whichever run last built that package under any feature set, which is the
/// staleness this is avoiding.
fn dep_files(stdout: &str) -> Vec<Utf8PathBuf> {
    let mut wanted: HashMap<Utf8PathBuf, HashSet<String>> = HashMap::default();

    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }

        let filenames = message.get("filenames").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);

        for filename in filenames.iter().filter_map(Value::as_str) {
            let path = Utf8Path::new(filename);

            let Some((directory, stem)) = path.parent().zip(path.file_stem()) else {
                continue;
            };

            let Some((_name, hash)) = stem.rsplit_once('-') else {
                continue;
            };

            let _added = wanted.entry(directory.to_owned()).or_default().insert(hash.to_owned());
        }
    }

    let mut found = Vec::new();

    for (directory, hashes) in wanted {
        let Ok(entries) = fs::read_dir(directory.as_std_path()) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };

            if path.extension() != Some("d") {
                continue;
            }

            let matched = path
                .file_stem()
                .and_then(|stem| stem.rsplit_once('-'))
                .is_some_and(|(_name, hash)| hashes.contains(hash));

            if matched {
                found.push(path);
            }
        }
    }

    found
}

/// Rewrites `\` to `/` so that a dep-info path compares equal to a discovered one on Windows.
fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

/// Runs one cargo build, stopping it if it outstays its budget.
///
/// Returns `None` when the budget ran out, having killed the build. Waiting for cargo to finish
/// and complaining afterwards would report a slow build accurately and a hung one never: the whole
/// run rests on this single compile, and there is no test harness behind it to notice that nothing
/// is happening.
///
/// The pipes are drained on their own threads. A build produces megabytes of JSON, and a pipe
/// holds about sixty-four kilobytes, so a caller that waits for exit before reading deadlocks
/// against a compiler blocked writing to it — which would look exactly like the hang this is meant
/// to catch.
fn compile(work: &Workspace, args: &[String], budget: Option<Duration>) -> Result<Option<Output>> {
    let mut child = work
        .cargo()
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|cause| error!("could not run cargo in `{}`", work.root).caused_by(cause))?;

    let stdout = child.stdout.take().map(read_pipe);
    let stderr = child.stderr.take().map(read_pipe);

    let Some(budget) = budget else {
        let status = child
            .wait()
            .map_err(|cause| error!("could not wait for cargo in `{}`", work.root).caused_by(cause))?;

        return Ok(Some(collect(status, stdout, stderr)));
    };

    let deadline = Instant::now() + budget;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(collect(status, stdout, stderr))),

            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();

                    return Ok(None);
                }

                thread::sleep(BUILD_POLL_INTERVAL);
            }

            Err(cause) => {
                return Err(error!("could not wait for cargo in `{}`", work.root).caused_by(cause));
            }
        }
    }
}

/// How often a running build is checked for having finished.
///
/// A build is measured in seconds at best, so a coarse poll costs nothing and keeps an otherwise
/// idle thread from spinning against a compiler that wants the core.
const BUILD_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Drains one pipe on its own thread.
fn read_pipe<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut text = Vec::new();
        let _read = pipe.read_to_end(&mut text);

        text
    })
}

/// Reassembles what the build said, once it has said all of it.
fn collect(status: ExitStatus, stdout: Option<JoinHandle<Vec<u8>>>, stderr: Option<JoinHandle<Vec<u8>>>) -> Output {
    let joined = |handle: Option<JoinHandle<Vec<u8>>>| handle.and_then(|handle| handle.join().ok()).unwrap_or_default();

    Output { status, stdout: joined(stdout), stderr: joined(stderr) }
}

/// What one cargo invocation produced.
#[derive(Debug)]
struct Compiled {
    succeeded: bool,

    /// Cargo's JSON stream, or `None` if the build was stopped for outstaying its budget.
    stdout: Option<String>,

    /// What cargo said on stderr.
    ///
    /// Kept because the JSON stream only carries what the *compiler* said, and a build can fail
    /// without the compiler ever being reached: a build script that panics, a missing native
    /// library, an unresolvable dependency, an ambiguous package. Those failures are explained on
    /// stderr and nowhere else, and discarding it left the reader with a build that failed for no
    /// stated reason.
    stderr: String,
}

/// Runs one cargo command in the tree under the build budget.
fn run_cargo(
    work: &Workspace,
    plan: &Plan,
    verb: &[&str],
    select: Option<&[String]>,
    limits: BuildLimits,
    first_round: Option<Duration>,
) -> Result<Compiled> {
    let mut args: Vec<String> = verb.iter().map(|arg| (*arg).to_owned()).collect();

    args.push("--message-format=json".to_owned());

    match select {
        Some(packages) => {
            for package in packages {
                args.push("--package".to_owned());
                args.push(plan.spec(&work.root, package));
            }
        }
        None => args.push("--workspace".to_owned()),
    }

    work.cargo.extend_build_args(&mut args);

    let Some(output) = compile(work, &args, limits.budget(first_round))? else {
        return Ok(Compiled { succeeded: false, stdout: None, stderr: String::new() });
    };

    // Cargo's JSON stream can run to many megabytes, and it is valid UTF-8 in every case that
    // matters, so the bytes are taken over rather than copied.
    let stdout = match String::from_utf8(output.stdout) {
        Ok(text) => text,
        Err(invalid) => String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
    };

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(Compiled { succeeded: output.status.success(), stdout: Some(stdout), stderr })
}

/// Groups every live mutant by the file it belongs to.
///
/// Scanning the whole population once per file is quadratic, and a large workspace has both many
/// files and many mutants, so the grouping is built once and shared. The rollback loop instruments
/// the tree again on every round, which multiplies whatever this costs.
fn by_file<'a>(mutants: &'a [Mutant], withdrawn: &HashSet<u32>) -> HashMap<&'a Utf8Path, Vec<&'a Mutant>> {
    let mut grouped: HashMap<&Utf8Path, Vec<&Mutant>> = HashMap::default();

    for mutant in mutants {
        if mutant.ordinal > 0 && !withdrawn.contains(&mutant.ordinal) {
            grouped.entry(mutant.file.as_path()).or_default().push(mutant);
        }
    }

    grouped
}

/// Writes the instrumented form of every mutated file into the copied tree.
///
/// Returns where each live mutant's guard landed, which is what attributes a compiler diagnostic
/// back to the mutant responsible.
fn instrument_tree(work: &Workspace, plan: &Plan, withdrawn: &HashSet<u32>) -> Result<Guards> {
    let mut guards = Guards::default();
    let grouped = by_file(&plan.mutants, withdrawn);

    for file in &plan.files {
        let live = grouped.get(file.path.as_path()).map_or(&[][..], Vec::as_slice);

        let destination = work.root.join(&file.path);
        let original = fs::read_to_string(file.absolute.as_std_path())
            .map_err(|cause| error!("could not read `{}`", file.absolute).caused_by(cause))?;

        // A file whose every mutant has been withdrawn still has to be rewritten, back to the
        // original, or the previous round's instrumented copy would survive its own withdrawal and
        // the rollback loop could never converge.
        let instrumented = if live.is_empty() {
            original
        } else {
            let (instrumented, found) = schema::instrument_with_guards(&original, live)?;

            for (ordinal, guard) in found {
                let _ = guards.insert(ordinal, (file.path.clone(), guard));
            }

            // A live mutant with no guard would still be run — with nothing in the tree to make it
            // behave differently — and its verdict recorded as a survivor. That is a wrong answer
            // rather than a missing one, and nothing downstream could tell the difference, so the
            // invariant is checked rather than assumed.
            if let Some(missing) = live.iter().find(|mutant| !guards.contains_key(&mutant.ordinal)) {
                return Err(Converger::missing_guard_error(missing));
            }

            instrumented
        };

        // Rewriting a file with the text it already holds would make cargo rebuild its crate, so
        // an unchanged file is left alone and its mtime with it.
        let _written = Workspace::overwrite(&destination, &instrumented)?;
    }

    Ok(guards)
}

/// Works out which mutants to blame for a failed build.
///
/// Guard positions come from the instrumented text rather than from the mutants' source lines,
/// because a guard emits the original text alongside the mutated one and so shifts every later
/// line. Only primary spans are considered: a diagnostic's notes routinely point at the innocent
/// declaration a mutated expression happened to misuse.
///
/// A diagnostic landing in some guard's mutated branch names its cause exactly, since that branch
/// is the only text in the tree that is not a copy of the original and no two of them overlap.
/// Failing that — a mutant can break code it merely encloses, and a deletion has no replacement
/// text to land in — the innermost guarded site containing the diagnostic is blamed instead.
/// Mutants sharing a site are withdrawn together, which can retire one that would have compiled;
/// it is reported as unviable rather than dropped.
fn blame(stdout: &str, root: &Utf8Path, guards: &Guards) -> HashSet<u32> {
    let mut blamed = HashSet::default();

    // A failing build reports many diagnostics and a large workspace has many guards, so pairing
    // them off one at a time is quadratic. Grouping by file first makes the common case a lookup.
    let mut by_path: HashMap<&Utf8Path, Vec<(u32, &Guard)>> = HashMap::default();

    for (ordinal, (file, guard)) in guards {
        by_path.entry(file.as_path()).or_default().push((*ordinal, guard));
    }

    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if message.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }

        let diagnostic = message.get("message").unwrap_or(&Value::Null);

        if diagnostic.get("level").and_then(Value::as_str) != Some("error") {
            continue;
        }

        let Some(spans) = diagnostic.get("spans").and_then(Value::as_array) else {
            continue;
        };

        let primary: Vec<&Value> = spans
            .iter()
            .filter(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
            .collect();

        let considered = if primary.is_empty() { spans.iter().collect() } else { primary };
        let mut exact = HashSet::default();
        let mut enclosing: Option<(u32, HashSet<u32>)> = None;
        let mut contained: Option<(u32, HashSet<u32>)> = None;

        for span in considered {
            let Some(file_name) = span.get("file_name").and_then(Value::as_str) else {
                continue;
            };

            let relative = Utf8Path::new(file_name)
                .strip_prefix(root.as_str())
                .unwrap_or_else(|_ignored| Utf8Path::new(file_name));

            let Some(reported) = position_range(span) else {
                continue;
            };

            // The exact relative path is the normal case; the scan is the fallback for a diagnostic
            // whose path is spelled differently, and only runs when the lookup found nothing.
            let matched = by_path.get(relative).map(Vec::as_slice).unwrap_or_default();
            let scanned;

            let here = if matched.is_empty() {
                scanned = by_path
                    .iter()
                    .filter(|(file, _found)| file_name.ends_with(file.as_str()))
                    .flat_map(|(_file, found)| found.iter().copied())
                    .collect::<Vec<_>>();

                scanned.as_slice()
            } else {
                matched
            };

            for (ordinal, guard) in here.iter().copied() {
                if guard.mutated.as_ref().is_some_and(|mutated| covers(mutated, &reported)) {
                    let _ = exact.insert(ordinal);
                } else if covers(&guard.site, &reported) {
                    let width = guard.site.end.line.saturating_sub(guard.site.start.line);

                    match &mut enclosing {
                        Some((best, ordinals)) if *best == width => {
                            let _ = ordinals.insert(ordinal);
                        }
                        Some((best, _ordinals)) if *best < width => {}
                        _ => enclosing = Some((width, HashSet::from_iter([ordinal]))),
                    }
                } else if covers(&reported, &guard.site) {
                    // The diagnostic encloses the guard rather than the other way round, which is
                    // what a borrow checker error looks like: the guard makes some subexpression
                    // non-constant and the complaint lands on the whole construct that depended on
                    // it. Every guard inside the smallest such region is a candidate, because
                    // nothing narrower distinguishes them.
                    let width = reported.end.line.saturating_sub(reported.start.line);

                    match &mut contained {
                        Some((best, ordinals)) if *best == width => {
                            let _ = ordinals.insert(ordinal);
                        }
                        Some((best, _ordinals)) if *best < width => {}
                        _ => contained = Some((width, HashSet::from_iter([ordinal]))),
                    }
                }
            }
        }

        // Preference runs from the most specific attribution to the least. The last is a blunt
        // instrument and can retire mutants that would have compiled, but the alternative is a
        // diagnostic nothing can be blamed for, which loses the entire run rather than a few
        // mutants that are then reported as unviable.
        let ordinals = if exact.is_empty() {
            enclosing.or(contained).map(|(_width, ordinals)| ordinals)
        } else {
            Some(exact)
        };

        if let Some(ordinals) = ordinals {
            blamed.extend(ordinals);
        }
    }

    blamed
}

/// Reads the region a compiler diagnostic points at out of its JSON span.
fn position_range(span: &Value) -> Option<Range<Position>> {
    let at = |line: &str, column: &str| {
        let line = span.get(line).and_then(Value::as_u64)?;
        let column = span.get(column).and_then(Value::as_u64)?;

        Some(Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            column: u32::try_from(column).unwrap_or(u32::MAX),
        })
    };

    Some(at("line_start", "column_start")?..at("line_end", "column_end")?)
}

/// Reports whether a guard's region wholly contains the one a diagnostic points at.
fn covers(range: &Range<Position>, reported: &Range<Position>) -> bool {
    range.start <= reported.start && range.end >= reported.end
}

/// Extracts the human-readable compiler diagnostics from cargo's JSON output.
///
/// With `--message-format=json` the diagnostics arrive on stdout as structured messages and
/// stderr carries only a summary, so a failure report built from stderr would not say what went wrong.
/// Keeps the part of cargo's stderr that explains a failure.
///
/// Cargo narrates its progress on the same stream it reports failures on, and a cold build narrates
/// thousands of lines. Handing all of that to someone whose build just failed buries the two lines
/// that matter, so the progress verbs are dropped and everything else is kept — including the
/// indented `Caused by` blocks and build-script output, which is usually where the real cause is.
fn complaints(stderr: &str) -> String {
    /// The verbs cargo uses to narrate work it is doing rather than trouble it has hit.
    const PROGRESS: [&str; 10] = [
        "Compiling",
        "Checking",
        "Downloading",
        "Downloaded",
        "Updating",
        "Locking",
        "Adding",
        "Finished",
        "Fresh",
        "Running",
    ];

    let mut kept = String::new();

    for line in stderr.lines() {
        let trimmed = line.trim_start();

        if PROGRESS.iter().any(|verb| {
            trimmed
                .strip_prefix(verb)
                .is_some_and(|rest| rest.starts_with(' ') || rest.is_empty())
        }) {
            continue;
        }

        if trimmed.is_empty() && kept.is_empty() {
            continue;
        }

        kept.push_str(line);
        kept.push('\n');
    }

    if kept.trim().is_empty() {
        return "cargo said nothing on stderr either.".to_owned();
    }

    kept
}

fn diagnostics(stdout: &str) -> String {
    let mut rendered = String::new();

    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if message.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }

        let Some(diagnostic) = message.get("message") else {
            continue;
        };

        if diagnostic.get("level").and_then(Value::as_str) != Some("error") {
            continue;
        }

        if let Some(text) = diagnostic.get("rendered").and_then(Value::as_str) {
            rendered.push_str(text);
        }
    }

    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Outcome;
    use crate::ops::collect::Shape;

    /// A workspace holding one trivial crate, so a real cargo invocation is cheap.
    fn trivial_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
        let dir = crate::testing::workdir(prefix);
        let root = Utf8PathBuf::from_path_buf(dir.path().join("src")).expect("utf8");

        fs::create_dir_all(root.join("src").as_std_path()).expect("src");
        fs::write(
            root.join("Cargo.toml").as_std_path(),
            "[package]\nname = \"trivial\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("manifest");
        fs::write(root.join("src/lib.rs").as_std_path(), "pub const A: i32 = 1;\n").expect("lib");

        let target = Utf8PathBuf::from_path_buf(dir.path().join("target")).expect("utf8");
        let work = Workspace::adopt(root, target);

        (dir, work)
    }

    /// A build that fails for a reason no guard explains stops rather than looping forever.
    #[test]
    fn a_build_that_no_guard_explains_stops_with_the_compiler_output() {
        let (_dir, work) = trivial_workspace("build-unattributed-");

        // Broken source that has nothing to do with instrumentation: no mutant can be withdrawn
        // to make it compile, so withdrawing forever would be an infinite loop.
        fs::write(work.root.join("src/lib.rs").as_std_path(), "pub const A: i32 = \"not an integer\";\n").expect("lib");

        let plan = empty_plan(&work);
        let limits = BuildLimits::default();
        let error = Converger::default()
            .converge(&work, &plan, None, &["build", "--tests", "--keep-going"], limits)
            .expect_err("the build must fail");

        assert!(error.to_string().contains("could not be attributed"), "{error}");
    }

    /// A narrowed build that fails is retried across the whole workspace before giving up.
    #[test]
    fn a_narrowed_build_that_fails_is_retried_across_the_whole_workspace() {
        let (_dir, work) = trivial_workspace("build-widen-");
        let mut plan = empty_plan(&work);
        let select = vec!["no-such-package".to_owned()];

        // Cargo rejects the selection outright, which is exactly the shape of failure the widen
        // path exists for: the narrowing is at fault, not the code being built.
        let build = Converger::default()
            .finish(&work, &mut plan, Some(&select), BuildLimits::default())
            .expect("widening to the whole workspace must succeed");

        assert!(build.widened, "the build should have reported that it widened");
    }

    /// A whole-workspace build that fails is reported as it stands, with nothing to widen to.
    #[test]
    fn a_whole_workspace_build_that_fails_is_reported_rather_than_retried() {
        let (_dir, work) = trivial_workspace("build-nowiden-");

        fs::write(work.root.join("src/lib.rs").as_std_path(), "pub const A: i32 = \"not an integer\";\n").expect("lib");

        let mut plan = empty_plan(&work);

        // There was no narrowing to blame, so there is no second build to try: retrying the same
        // command would only spend the time again to reach the same answer.
        let error = Converger::default()
            .finish(&work, &mut plan, None, BuildLimits::default())
            .expect_err("the build must fail");

        assert!(error.to_string().contains("could not be attributed"), "{error}");
    }

    /// A plan with no mutants and no files, rooted in the given workspace.
    fn empty_plan(work: &Workspace) -> Plan {
        Plan {
            root: work.root.clone(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    /// A build that outstays its budget is killed rather than waited out.
    #[test]
    fn a_build_that_outstays_its_budget_is_stopped() {
        let (_dir, work) = trivial_workspace("build-budget-");

        // Zero budget: the deadline has passed before the first poll, so the child is killed on the
        // very first pass through the wait loop.
        let outcome = compile(&work, &["check".to_owned()], Some(Duration::ZERO)).expect("spawn");

        assert!(outcome.is_none(), "a build past its budget should report no output");
    }

    /// A build stopped by its budget reports no output at all, so the caller can say so.
    #[test]
    fn a_build_stopped_by_its_budget_reports_no_stdout() {
        let (_dir, work) = trivial_workspace("build-nostdout-");
        let limits = BuildLimits {
            timeout: Some(Duration::ZERO),
            multiplier: None,
            rollback_rounds: 0,
        };

        let compiled = run_cargo(&work, &empty_plan(&work), &["check"], None, limits, None).expect("spawn");

        assert!(!compiled.succeeded);
        assert!(compiled.stdout.is_none());
    }

    /// And converging on such a build stops with an error naming the budget rather than looping.
    #[test]
    fn converging_on_a_build_that_never_finishes_stops_with_the_budget() {
        let (_dir, work) = trivial_workspace("build-converge-budget-");
        let plan = Plan {
            root: work.root.clone(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        };
        let limits = BuildLimits {
            timeout: Some(Duration::ZERO),
            multiplier: None,
            rollback_rounds: 0,
        };

        let error = Converger::default()
            .converge(&work, &plan, None, &["check"], limits)
            .expect_err("the build never finishes");

        assert!(error.to_string().contains("was still running"), "{error}");
        assert!(error.to_string().contains("--build-timeout"), "{error}");
    }

    /// A build inside its budget is collected through the same polling wait.
    #[test]
    fn a_build_inside_its_budget_is_collected() {
        let (_dir, work) = trivial_workspace("build-collected-");

        let outcome = compile(&work, &["--version".to_owned()], Some(Duration::from_secs(120)))
            .expect("spawn")
            .expect("cargo should finish well inside two minutes");

        assert!(outcome.status.success(), "{outcome:?}");
    }

    /// A cargo that cannot be spawned at all names the tree it was to run in.
    #[test]
    fn a_cargo_that_cannot_be_spawned_names_the_tree() {
        let work = Workspace::adopt(
            Utf8PathBuf::from("/gamma/definitely/not/a/directory"),
            Utf8PathBuf::from("/gamma/definitely/not/a/directory/target"),
        );

        let error = compile(&work, &["--version".to_owned()], None).expect_err("no such directory");

        assert!(error.to_string().contains("could not run cargo"), "{error}");
    }

    fn at(line: u32, column: u32) -> Position {
        Position { line, column }
    }

    fn guard(site: Range<Position>, mutated: Option<Range<Position>>) -> Guard {
        Guard { site, mutated }
    }

    fn span(file: &str, line_start: u32, column_start: u32, line_end: u32, column_end: u32, primary: bool) -> Value {
        serde_json::json!({
            "file_name": file,
            "line_start": line_start,
            "column_start": column_start,
            "line_end": line_end,
            "column_end": column_end,
            "is_primary": primary,
        })
    }

    fn compiler_message(spans: &[Value]) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "rendered": "error: boom\n",
                "spans": spans,
            },
        })
        .to_string()
    }

    fn mutant() -> Mutant {
        Mutant {
            id: "deadbeefcafe".to_owned(),
            ordinal: 1,
            file: Utf8PathBuf::from("src/lib.rs"),
            package: "pkg".to_owned(),
            span: 0..1,
            line: 7,
            column: 3,
            mutator: "lit.true_to_false".to_owned(),
            item_path: "pkg::f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "true".to_owned(),
            replacement: "false".to_owned(),
            shape: Shape::Expr,
            outcome: Outcome::Pending,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    #[test]
    fn diagnostics_are_read_from_the_json_stream() {
        // Diagnostics arrive on stdout as JSON; stderr holds only a summary, so a failure report
        // built from stderr would say nothing about what actually went wrong.
        let stdout = concat!(
            r#"{"reason":"compiler-message","message":{"level":"error","rendered":"error[E0308]: boom"}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"warning","rendered":"just a warning"}}"#,
            "\n",
            r#"{"reason":"compiler-message"}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{}}"#,
            "\n",
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/x"}"#,
            "\n",
            // Cargo interleaves its own plain-text chatter with the JSON stream.
            "warning: unused manifest key\n",
        );
        let rendered = diagnostics(stdout);

        assert!(rendered.contains("E0308"));
        assert!(!rendered.contains("just a warning"));
        assert!(!rendered.contains("unused manifest key"));
    }

    #[test]
    fn diagnostic_spans_inside_mutated_text_name_that_mutant_exactly() {
        let mut guards = Guards::default();
        let file = Utf8PathBuf::from("src/lib.rs");

        let _old = guards.insert(
            1,
            (
                file.clone(),
                guard(at(10, 1)..at(20, 1), Some(at(12, 5)..at(12, 10))),
            ),
        );
        let _old = guards.insert(2, (file, guard(at(10, 1)..at(20, 1), None)));

        // Exact mutated-branch hits are trusted before the broader guarded site, or shared sites
        // would withdraw innocent neighbors.
        let blamed = blame(&compiler_message(&[span("src/lib.rs", 12, 6, 12, 8, true)]), Utf8Path::new("/work"), &guards);

        assert_eq!(blamed, HashSet::from_iter([1]));
    }

    #[test]
    fn a_diagnostic_inside_a_guard_blames_the_innermost_enclosing_site() {
        let mut guards = Guards::default();

        for ordinal in [1, 2, 3] {
            let site = if ordinal == 1 { at(1, 1)..at(50, 1) } else { at(10, 1)..at(15, 1) };
            let _old = guards.insert(ordinal, (Utf8PathBuf::from("src/lib.rs"), guard(site, None)));
        }

        // When a deletion or type change breaks copied text inside the guard, the narrowest site
        // is the least destructive rollback candidate, with ties kept together.
        let blamed = blame(&compiler_message(&[span("src/lib.rs", 12, 2, 12, 4, true)]), Utf8Path::new("/work"), &guards);

        assert_eq!(blamed, HashSet::from_iter([2, 3]));
    }

    #[test]
    fn a_diagnostic_that_encloses_guards_keeps_only_the_smallest_reported_region() {
        let mut guards = Guards::default();

        let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(20, 1)..at(21, 1), None)));
        let _old = guards.insert(2, (Utf8PathBuf::from("src/lib.rs"), guard(at(40, 1)..at(41, 1), None)));

        let stdout = compiler_message(&[
            span("src/lib.rs", 1, 1, 100, 1, false),
            span("src/lib.rs", 35, 1, 45, 1, false),
        ]);

        // Borrow-checker errors can land on a construct containing a guard; the smallest such
        // diagnostic is blamed so a broad fallback does not mask a narrower one.
        let blamed = blame(&stdout, Utf8Path::new("/work"), &guards);

        assert_eq!(blamed, HashSet::from_iter([2]));
    }

    #[test]
    fn diagnostics_are_matched_by_suffix_when_cargo_spells_paths_differently() {
        let mut guards = Guards::default();

        let _old = guards.insert(
            1,
            (
                Utf8PathBuf::from("crates/pkg/src/lib.rs"),
                guard(at(5, 1)..at(6, 1), Some(at(5, 5)..at(5, 10))),
            ),
        );

        // Cargo can report an absolute or otherwise differently-rooted path; suffix matching keeps
        // those diagnostics attributable rather than losing the whole run.
        let blamed = blame(
            &compiler_message(&[span("/elsewhere/crates/pkg/src/lib.rs", 5, 6, 5, 8, true)]),
            Utf8Path::new("/scratch/tree"),
            &guards,
        );

        assert_eq!(blamed, HashSet::from_iter([1]));
    }

    #[test]
    fn non_error_messages_and_malformed_spans_are_ignored_for_blame() {
        let mut guards = Guards::default();

        let _old = guards.insert(1, (Utf8PathBuf::from("src/lib.rs"), guard(at(1, 1)..at(2, 1), None)));
        let stdout = [
            "not json".to_owned(),
            serde_json::json!({"reason": "compiler-artifact"}).to_string(),
            serde_json::json!({"reason": "compiler-message", "message": {"level": "warning"}}).to_string(),
            serde_json::json!({"reason": "compiler-message", "message": {"level": "error"}}).to_string(),
            serde_json::json!({
                "reason": "compiler-message",
                "message": {"level": "error", "spans": [{}, {"file_name": "src/lib.rs"}]},
            })
            .to_string(),
        ]
        .join("\n");

        // Ignoring incomplete compiler output is safer than guessing and withdrawing unrelated
        // mutants.
        assert!(blame(&stdout, Utf8Path::new("/work"), &guards).is_empty());
    }

    #[test]
    fn build_errors_are_formatted_without_running_cargo() {
        let timeout = Converger::build_timeout_error(Duration::from_secs(2)).to_string();
        let stdout = compiler_message(&[span("src/lib.rs", 1, 1, 1, 2, true)]);
        let (_dir, mut work) = trivial_workspace("build-errors-");

        work.leak = true;

        let unattributed = Converger::unattributed_build_error(&work, &stdout, "").to_string();
        let limited = Converger::rollback_limit_error(32, &[9, 5, 2], &work, &stdout).to_string();
        let missing = Converger::missing_guard_error(&mutant()).to_string();

        // These messages are the user's only explanation of build failures that happen before any
        // test binary can run, so the pure formatting paths are kept under test.
        assert!(timeout.contains("after 2s"));
        assert!(unattributed.contains("could not be attributed"));
        assert!(limited.contains("32 rounds"));
        assert!(limited.contains("16 withdrawn in total"));
        assert!(limited.contains("9, 5, 2"), "{limited}");

        // A round that withdrew nothing means the failure is not a mutant's fault, so the advice
        // has to say that rather than suggesting more rounds.
        let stuck = Converger::rollback_limit_error(4, &[3, 0], &work, &stdout).to_string();

        assert!(stuck.contains("more rounds will not help"), "{stuck}");
        assert!(missing.contains("no guard was emitted"));
        assert!(missing.contains("src/lib.rs:7"));

        // A leaked tree is still there to be read, so the message names it.
        assert!(unattributed.contains(work.root.as_str()), "{unattributed}");

        // One that was not leaked is gone by the time the message is read, and sending someone to
        // a path that does not exist reads as a second bug on top of the one being reported.
        work.leak = false;

        let swept = Converger::unattributed_build_error(&work, &stdout, "").to_string();

        assert!(swept.contains("--leak-dirs"), "{swept}");
        assert!(!swept.contains(work.root.as_str()), "{swept}");
    }

    #[test]
    fn a_build_that_reported_nothing_is_not_described_as_a_compile_failure() {
        // Cargo rejects a bad invocation — an ambiguous `--package`, an unknown feature — before it
        // compiles anything, so the failure arrives with an empty JSON stream. Calling that "does
        // not compile" sends the reader hunting for a broken mutant that was never generated.
        let (_dir, work) = trivial_workspace("silent-build-");
        let stderr = "   Compiling tonic v0.14.0\n\
                      error: failed to run custom build command for `codegen v0.1.0`\n\n\
                      Caused by:\n  \
                        process didn't exit successfully: exit status: 101\n";
        let message = Converger::unattributed_build_error(&work, "", stderr).to_string();

        assert!(message.contains("the compiler reported nothing"), "{message}");
        assert!(!message.contains("does not compile"), "{message}");

        // The whole point: the cause is on stderr and nowhere else, so it has to be shown.
        assert!(message.contains("failed to run custom build command"), "{message}");
        assert!(message.contains("Caused by"), "{message}");

        // Cargo narrates its progress on the same stream, and a cold build narrates thousands of
        // lines. Those must not bury the two that matter.
        assert!(!message.contains("Compiling tonic"), "{message}");
    }

    #[test]
    fn a_failure_with_nothing_on_either_stream_says_so_rather_than_showing_a_blank() {
        // Printing an empty block reads as though the message itself is broken.
        let (_dir, work) = trivial_workspace("silent-both-");
        let message = Converger::unattributed_build_error(&work, "", "   Compiling x v0.1.0\n").to_string();

        assert!(message.contains("cargo said nothing on stderr either"), "{message}");
    }

    #[test]
    fn cargo_progress_is_told_apart_from_a_word_that_merely_starts_the_same_way() {
        // `Compiling` is progress; `Compilation failed` is not, and dropping it would hide the
        // only line that explains the failure.
        let kept = complaints("   Compiling x v0.1.0\nCompilationfailed for a reason\n");

        assert!(kept.contains("Compilationfailed"), "{kept}");
        assert!(!kept.contains("Compiling x"), "{kept}");
    }

    /// Dep-info from an earlier run with different features is the whole reason this is keyed off
    /// the build's own artifact messages, so it has to be shown that the stale file is ignored.
    #[test]
    fn only_the_dep_info_belonging_to_this_build_is_read() {
        let dir = crate::testing::workdir("dep-files-");
        let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

        fs::create_dir_all(deps.as_std_path()).expect("deps");
        fs::write(deps.join("mine-aaaa.d").as_std_path(), "x: src/kept.rs\n").expect("mine");
        fs::write(deps.join("stale-bbbb.d").as_std_path(), "x: src/gone.rs\n").expect("stale");

        let stdout = format!(r#"{{"reason":"compiler-artifact","filenames":["{deps}/libmine-aaaa.rmeta"]}}"#);

        let compiled = compiled_sources(&stdout, Utf8Path::new("/nowhere")).expect("read something");

        assert!(compiled.contains(Utf8Path::new("src/kept.rs")), "{compiled:?}");
        assert!(!compiled.contains(Utf8Path::new("src/gone.rs")), "{compiled:?}");
    }

    /// An unreadable tree must yield no opinion at all. Returning an empty set instead would mark
    /// every mutant in the run as unbuilt and report a perfect score for a run that tested nothing.
    #[test]
    fn dep_info_that_cannot_be_read_yields_no_conclusion() {
        let stdout = r#"{"reason":"compiler-artifact","filenames":["/nowhere/deps/libmine-aaaa.rmeta"]}"#;

        assert!(compiled_sources(stdout, Utf8Path::new("/nowhere")).is_none());
    }

    /// Dep-info spells its paths absolutely for some units and relatively for others, and mutants
    /// are only ever named relative to the tree, so both spellings have to arrive the same way.
    #[test]
    fn absolute_and_relative_dependency_paths_both_land_relative_to_the_tree() {
        let dir = crate::testing::workdir("dep-paths-");
        let deps = Utf8PathBuf::from_path_buf(dir.path().join("deps")).expect("utf8");

        fs::create_dir_all(deps.as_std_path()).expect("deps");
        fs::write(deps.join("mine-aaaa.d").as_std_path(), "x: /tree/src/one.rs src/two.rs\n").expect("mine");

        let stdout = format!(r#"{{"reason":"compiler-artifact","filenames":["{deps}/mine-aaaa.rlib"]}}"#);
        let compiled = compiled_sources(&stdout, Utf8Path::new("/tree")).expect("read something");

        assert!(compiled.contains(Utf8Path::new("src/one.rs")), "{compiled:?}");
        assert!(compiled.contains(Utf8Path::new("src/two.rs")), "{compiled:?}");
    }
}
