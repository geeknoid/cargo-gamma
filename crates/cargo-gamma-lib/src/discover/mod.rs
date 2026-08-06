//! Finding the workspace, its packages, and the source files worth mutating.

mod diff;
mod modules;
mod order;
mod plan;
mod target_file;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{CargoOpt, Metadata, MetadataCommand, Target};
use core::num::NonZero;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::panic::resume_unwind;
use std::thread;
use walkdir::WalkDir;

use crate::Result;
use crate::cfg::{CfgSet, Cfgs, features};
use crate::commands::{FeatureArgs, SelectArgs};
use crate::error::{Error, error};
use crate::model::{Mutant, Outcome};
use crate::ops::collect;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;
use crate::suppress;

pub use diff::Diff;
pub(crate) use order::stages;
pub use plan::Plan;
pub use target_file::TargetFile;

/// Builds the plan for a run.
///
/// `notify` is called with a short human-readable message, so the caller can drive a progress
/// display without this module knowing anything about terminals. It is called once with the size
/// of the job before the files are parsed, and once per package afterwards carrying what that
/// package actually yielded — the counts do not exist until the parse is done.
pub fn plan(
    args: &SelectArgs,
    selection: &Selection,
    shard: Option<(u32, u32)>,
    notify: &mut impl FnMut(&str),
) -> Result<Plan> {
    let survey = Survey::new(args, shard)?;

    // Parsing is the expensive half of discovery and says nothing while it runs, so its size is
    // announced before it starts rather than leaving the display silent.
    notify(&format!("{} for mutants", crate::report::quantity(survey.files.len(), "file")));

    let mut ordinals = 0;
    let scanned = survey.scan(None, selection, &mut ordinals)?;

    report_by_package(&survey.files, &scanned.mutants, notify);

    Ok(survey.into_plan(scanned))
}

/// The workspace, its files and its shape, worked out without parsing a line of source.
///
/// Discovery divides in two. Working out which files are worth mutating costs a cargo metadata
/// call and a directory walk; parsing them costs far more. Splitting the two lets a run copy the
/// workspace and then scan, instrument and build one package at a time, rather than parsing
/// everything before anything else can start.
#[derive(Debug)]
pub struct Survey {
    /// Absolute path of the workspace root.
    pub root: Utf8PathBuf,

    /// Every file worth mutating, sorted by path.
    pub files: Vec<TargetFile>,

    /// For each workspace package, the workspace packages its test binaries can reach.
    pub reach: crate::HashMap<String, crate::HashSet<String>>,

    /// Every test target the workspace declares, by name, sorted and deduplicated.
    ///
    /// This is the population `--include-test` and `--exclude-test` are checked against, and it is
    /// deliberately wider than the set of binaries a given run builds. A target gated behind
    /// `required-features` is declared here and compiled only when those features are on, so
    /// checking against what was built would reject a pattern in `gamma.toml` on every run that
    /// did not happen to enable them — which is precisely the run the pattern exists to survive.
    /// Collected across every workspace member, since `--package` chooses what to mutate while
    /// these patterns choose what judges it.
    pub tests: Vec<String>,

    /// For each package, its crate roots — the lib and bin entry points the module tree hangs off.
    roots: crate::HashMap<String, Vec<Utf8PathBuf>>,

    /// For each package, where its manifest sits relative to the workspace root, and its version.
    ///
    /// A bare `--package name` is ambiguous whenever a workspace member shares its name with a
    /// crate in the dependency graph, which happens routinely: a crate that dev-depends on a
    /// published version of itself, or two members of a graph that both vendor a common name.
    /// Cargo then refuses the build, and refuses it *before* producing any JSON, so the failure
    /// arrives with no diagnostics to attribute and looks like the tree simply not compiling.
    /// Keeping the manifest location lets every build name its packages exactly.
    specs: crate::HashMap<String, (Utf8PathBuf, String)>,

    /// For each package, the configuration predicates that hold when it is built.
    ///
    /// Code the compiler will strip produces no mutants, because a guard there is never compiled
    /// and no test could activate it. Resolved once here rather than per file, since a `rustc`
    /// call and a feature closure per source file would dominate discovery.
    cfgs: Cfgs,

    diff: Option<Diff>,
    shard: Option<(u32, u32)>,
    settled: crate::HashSet<String>,
}

/// What scanning some part of the workspace yielded.
#[derive(Debug, Default)]
pub struct Scanned {
    /// The mutants found, with ordinals already assigned to the live ones.
    pub mutants: Vec<Mutant>,

    /// How many were suppressed by a directive. They stay in `mutants`, marked as ignored.
    pub suppressed: usize,

    /// How many live mutants sharding excluded. These are counted rather than kept.
    pub sharded_out: usize,

    /// How many mutants an earlier report had already settled. Counted rather than kept.
    pub settled_out: usize,
}

impl Survey {
    /// Finds the workspace and the files worth mutating, without parsing any of them.
    ///
    /// # Errors
    ///
    /// Returns an error if cargo metadata cannot be read, a named package does not exist, or the
    /// diff cannot be parsed.
    pub fn new(args: &SelectArgs, shard: Option<(u32, u32)>) -> Result<Self> {
        let metadata = load_metadata(&args.dir, &args.features)?;
        let root = Utf8PathBuf::from(metadata.workspace_root.as_str());
        let mut files: Vec<TargetFile> = Vec::new();
        let mut seen: crate::HashSet<Utf8PathBuf> = crate::HashSet::default();
        let mut roots: crate::HashMap<String, Vec<Utf8PathBuf>> = crate::HashMap::default();
        let mut specs: crate::HashMap<String, (Utf8PathBuf, String)> = crate::HashMap::default();

        let diff = args.in_diff.as_ref().map(|path| Diff::read(path)).transpose()?;

        // Only collected when there is something to check, since it is one string per Rust file in
        // the workspace and the overwhelmingly common case has no patterns at all.
        let checking_patterns = !args.files.is_empty() || !args.exclude_files.is_empty();
        let mut walked: Vec<Utf8PathBuf> = Vec::new();

        if let Some(named) = unknown_packages(&metadata, args) {
            return Err(error!("no package named `{named}` in this workspace").usage());
        }

        for package in metadata.workspace_packages() {
            let mutating = args.mutates_package(package.name.as_str());

            // A package this run does not mutate is still walked when patterns need checking. The
            // patterns usually live in `gamma.toml` and are written once for the whole workspace,
            // whereas `--package` narrows a single run; validating them against the narrowed set
            // would reject a correct config on every run that happened to select another package.
            if !mutating && !checking_patterns {
                continue;
            }

            if mutating && let Some(directory) = Utf8Path::new(package.manifest_path.as_str()).parent() {
                let relative = directory.strip_prefix(&root).unwrap_or_else(|_outside| Utf8Path::new("")).to_owned();
                let _replaced = specs.insert(package.name.clone(), (relative, package.version.to_string()));
            }

            for target in &package.targets {
                if !is_mutable_target(target) {
                    continue;
                }

                let source_root = Utf8Path::new(target.src_path.as_str());

                // The module tree is walked from here to find the files that exist only for tests,
                // so a root is recorded whether or not it survives the filters below: a crate root
                // excluded from mutation still says what the rest of the crate is.
                if mutating {
                    roots
                        .entry(package.name.clone())
                        .or_default()
                        .push(source_root.to_owned());
                }

                let Some(directory) = source_root.parent() else {
                    continue;
                };

                for absolute in walk_rust_files(directory) {
                    let relative =
                        Utf8PathBuf::from(normalize_separators(absolute.strip_prefix(&root).unwrap_or(&absolute).as_str()));

                    if checking_patterns {
                        walked.push(relative.clone());
                    }

                    if !mutating {
                        continue;
                    }

                    if !is_included(&relative, args) {
                        continue;
                    }

                    // A file the diff does not mention cannot contain a changed line, so it is
                    // dropped before it is parsed rather than after, which is most of what makes
                    // `--in-diff` fast enough to run on every pull request.
                    if diff.as_ref().is_some_and(|diff| !diff.touches_file(&relative)) {
                        continue;
                    }

                    // A lib and a bin target in one package usually share a source directory, so
                    // the same file is walked more than once. A set rather than a scan of what is
                    // already held, because a large workspace makes that scan quadratic.
                    if !seen.insert(absolute.clone()) {
                        continue;
                    }

                    files.push(TargetFile {
                        path: relative,
                        absolute,
                        package: package.name.clone(),
                    });
                }
            }
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));

        if let Some(pattern) = unmatched_pattern(&walked, args) {
            return Err(error!(
                "no source file matches `{pattern}`; patterns are relative to the workspace root and use `/` on every platform"
            )
            .usage());
        }

        Ok(Self {
            root,
            files,
            reach: reachable(&metadata),
            tests: test_targets(&metadata),
            roots,
            specs,
            cfgs: configuration(&metadata, &args.features),
            diff,
            shard,
            settled: crate::HashSet::default(),
        })
    }

    /// Drops mutants an earlier report already settled, before they are given ordinals.
    ///
    /// Filtering here rather than on the finished plan keeps the counts a package reports about
    /// itself honest: a mutant that will not be run should not be numbered, instrumented, or
    /// announced as work.
    pub fn settle(&mut self, settled: crate::HashSet<String>) {
        self.settled = settled;
    }

    /// The workspace packages that have files worth mutating, in a stable order.
    #[must_use]
    pub fn packages(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::new();
        let mut seen: crate::HashSet<&str> = crate::HashSet::default();

        for file in &self.files {
            if seen.insert(file.package.as_str()) {
                order.push(file.package.clone());
            }
        }

        order
    }

    /// An empty plan for this workspace, to be filled in a package at a time.
    #[must_use]
    pub fn skeleton(&self) -> Plan {
        Plan {
            root: self.root.clone(),
            files: self.files.clone(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: self.reach.clone(),
            specs: self.specs.clone(),
        }
    }

    /// Parses and mutates one package's files, or every file when `package` is `None`.
    ///
    /// `ordinals` carries the last ordinal handed out, so that packages scanned one after another
    /// number their mutants continuously. Ordinals name the live mutants to the guard runtime, so
    /// they have to be unique across the whole run, not within a package.
    ///
    /// # Errors
    ///
    /// Returns an error if a file cannot be read or parsed.
    pub fn scan(&self, package: Option<&str>, selection: &Selection, ordinals: &mut u32) -> Result<Scanned> {
        let files: Vec<&TargetFile> = package.map_or_else(
            || self.files.iter().collect(),
            |wanted| self.files.iter().filter(|file| file.package == wanted).collect(),
        );

        let roots: Vec<Utf8PathBuf> = package.map_or_else(
            || self.roots.values().flatten().cloned().collect(),
            |wanted| self.roots.get(wanted).cloned().unwrap_or_default(),
        );
        let (mut mutants, suppressed) = scan(&files, &roots, selection, &self.cfgs)?;

        // Within a file the diff still has the last word: a changed line usually sits among many
        // that were not touched, and mutating those would report on code the change never went
        // near. A mutant is selected by the line its site starts on, which is the line the report
        // will name.
        if let Some(diff) = self.diff.as_ref() {
            mutants.retain(|mutant| {
                let line = u32::try_from(mutant.line).unwrap_or(u32::MAX);

                diff.touches(&mutant.file, line, line)
            });
        }

        // A mutant an earlier run already settled is dropped before anything else looks at it. It
        // takes no ordinal and no shard slot, so an iterative run reports on what it actually
        // looked at rather than on the population the first run faced.
        let before_settled = mutants.len();

        if !self.settled.is_empty() {
            mutants.retain(|mutant| !self.settled.contains(&mutant.id));
        }

        let settled_out = before_settled - mutants.len();

        // Suppressed mutants are kept but never run, so they take no part in sharding: letting
        // them occupy shard slots would make one night's shard cheaper than another for no reason,
        // and would hide how much of the population is actually being exercised.
        let is_live = |mutant: &Mutant| mutant.outcome != Outcome::Ignored;
        let before = mutants.iter().filter(|mutant| is_live(mutant)).count();

        if let Some((count, index)) = self.shard {
            mutants.retain(|mutant| !is_live(mutant) || shard_of(&mutant.id, count) == index);
        }

        let mut live = 0_usize;

        for mutant in &mut mutants {
            if is_live(mutant) {
                live = live.saturating_add(1);
                *ordinals = ordinals.saturating_add(1);
                mutant.ordinal = *ordinals;
            }
        }

        Ok(Scanned { mutants, suppressed, sharded_out: before - live, settled_out })
    }

    /// Turns a scan of the whole workspace into the plan a run works from.
    #[must_use]
    pub fn into_plan(self, scanned: Scanned) -> Plan {
        let mut plan = Plan {
            root: self.root,
            files: self.files,
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: self.reach,
            specs: self.specs,
        };

        plan.absorb(scanned);
        plan.sort();

        plan
    }
}

/// Reports what each package yielded, once the counts exist.
///
/// Package order follows the files, which are already sorted by path, so the same workspace always
/// reports in the same order. A package that produced no mutants is still named: a crate that
/// silently contributes nothing to a run is worth noticing, and its absence from the list would
/// look like it had simply not been looked at.
fn report_by_package(files: &[TargetFile], mutants: &[Mutant], notify: &mut impl FnMut(&str)) {
    let mut order: Vec<&str> = Vec::new();
    let mut counts: crate::HashMap<&str, (usize, usize)> = crate::HashMap::default();

    for file in files {
        let entry = counts.entry(file.package.as_str()).or_insert_with(|| {
            order.push(file.package.as_str());

            (0, 0)
        });

        entry.0 += 1;
    }

    for mutant in mutants {
        if let Some(entry) = counts.get_mut(mutant.package.as_str()) {
            entry.1 += 1;
        }
    }

    for package in order {
        let (files, mutants) = counts.get(package).copied().unwrap_or((0, 0));

        notify(&format!(
            "{package}, {} in {}",
            crate::report::quantity(mutants, "mutant"),
            crate::report::quantity(files, "file")
        ));
    }
}

/// Reads, parses and mutates every file, returning the population and how much of it was suppressed.
///
/// Parsing is what discovery actually spends its time on — a syntax tree per file, and nothing in
/// one file informs another — so the files are divided across the available cores. Work is claimed
/// one file at a time rather than in fixed blocks, since files vary enormously in size and a static
/// split leaves the machine waiting on whichever worker drew the largest ones. Results are put back
/// in file order afterwards, so the population does not depend on how the work happened to land.
fn scan(files: &[&TargetFile], roots: &[Utf8PathBuf], selection: &Selection, cfgs: &Cfgs) -> Result<(Vec<Mutant>, usize)> {
    let workers = thread::available_parallelism().map_or(1, NonZero::get).min(files.len().max(1));
    let next = AtomicUsize::new(0);

    let scan_one = |file: &TargetFile| -> Result<Parsed> {
        let mut source = SourceFile::read(&file.absolute)?;

        // Report paths relative to the workspace root; that is what a user can act on and what a
        // suppression or an expectation is keyed by.
        source.path = file.path.clone();

        // Taken from the tree that was parsed for mutants anyway, so knowing which files exist only
        // for tests costs a walk over the top-level items rather than a second parse of everything.
        let declared = modules::declarations(&file.absolute, &source.ast);
        let candidates = collect::collect_in(&source, selection, cfgs.for_package(&file.package));
        let mut found = collect::into_mutants(&source, &file.package, candidates);
        let directives = suppress::directives(&source)?;
        let suppressed = suppress::suppress(&mut found, &directives);

        Ok(Parsed { mutants: found, suppressed, declared })
    };

    let mut collected: Vec<(usize, Parsed)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_worker| {
                let next = &next;
                let scan_one = &scan_one;

                scope.spawn(move || {
                    let mut mine = Vec::new();

                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(file) = files.get(index) else { break };

                        match scan_one(file) {
                            Ok(parsed) => mine.push((index, parsed)),
                            Err(error) => return Err((index, error)),
                        }
                    }

                    Ok(mine)
                })
            })
            .collect();

        let mut collected = Vec::new();
        let mut failure: Option<(usize, Error)> = None;

        for handle in handles {
            // A panic in a worker is a bug in this crate, not something a user can act on, so it
            // is propagated rather than turned into a diagnostic that blames their code.
            match handle.join().unwrap_or_else(|payload| resume_unwind(payload)) {
                Ok(mine) => collected.extend(mine),

                // Several files can be unreadable or unparseable at once, and which worker noticed
                // first is a race. The earliest in file order is reported so the message does not
                // change between runs.
                Err((at, error)) => {
                    if failure.as_ref().is_none_or(|(seen, _)| at < *seen) {
                        failure = Some((at, error));
                    }
                }
            }
        }

        match failure {
            Some((_at, error)) => Err(error),
            None => Ok(collected),
        }
    })?;

    collected.sort_by_key(|(index, _parsed)| *index);

    // A file that turns out to be reachable only through `#[cfg(test)]` is test code, whatever it
    // looks like from the inside, and its mutants are dropped rather than reported. They cannot be
    // caught in any meaningful sense: a mutated assertion is a broken test, not a gap in one.
    let declared: Vec<(Utf8PathBuf, Vec<modules::Declaration>)> = collected
        .iter()
        .map(|(index, parsed)| {
            let path = files.get(*index).map_or_else(Utf8PathBuf::new, |file| file.absolute.clone());

            (path, parsed.declared.clone())
        })
        .collect();
    let excluded = modules::test_only(roots, &declared);
    let total = collected.iter().map(|(_index, parsed)| parsed.mutants.len()).sum();
    let mut mutants = Vec::with_capacity(total);
    let mut suppressed = 0;

    for (index, parsed) in collected {
        if files.get(index).is_some_and(|file| excluded.contains(&file.absolute)) {
            continue;
        }

        mutants.extend(parsed.mutants);
        suppressed += parsed.suppressed;
    }

    Ok((mutants, suppressed))
}

/// What one file yielded when it was parsed.
struct Parsed {
    mutants: Vec<Mutant>,
    suppressed: usize,
    declared: Vec<modules::Declaration>,
}

/// Works out which workspace packages each workspace package can reach.
///
/// Built from the declared dependencies rather than from a resolved graph, so it costs nothing
/// beyond the metadata already loaded. Dependencies of every kind count, including dev: an
/// integration test links its package's dev-dependencies, and being over-inclusive here can only
/// cost time, never correctness — the reverse would silently skip a test that really does reach the
/// mutated code and turn a survivor into a false clean bill of health.
///
/// The metadata is loaded with `--no-deps`, so a dependency that is not itself a workspace member
/// has no entry to walk into. A registry dependency cannot lead back into the workspace and can be
/// ignored, but a *path* dependency outside the workspace can: `app -> facade -> core` is a real
/// chain that this graph cannot see. Rather than skip a test binary that does reach the mutated
/// code, a package with such a dependency reaches everything.
fn reachable(metadata: &Metadata) -> crate::HashMap<String, crate::HashSet<String>> {
    let members: crate::HashSet<String> =
        metadata.workspace_packages().iter().map(|package| package.name.as_str().to_owned()).collect();

    let mut edges: crate::HashMap<String, Vec<String>> = crate::HashMap::default();
    let mut opaque: crate::HashSet<String> = crate::HashSet::default();

    for package in metadata.workspace_packages() {
        let mut neighbours = Vec::new();

        for dependency in &package.dependencies {
            if members.contains(&dependency.name) {
                neighbours.push(dependency.name.clone());
            } else if dependency.path.is_some() {
                let _inserted = opaque.insert(package.name.as_str().to_owned());
            }
        }

        let _previous = edges.insert(package.name.as_str().to_owned(), neighbours);
    }

    let mut reach: crate::HashMap<String, crate::HashSet<String>> = crate::HashMap::default();

    for start in &members {
        // A package whose graph cannot be walked in full reaches everything, because proving it
        // does *not* reach a package is exactly what the missing edges make impossible.
        if opaque.contains(start) {
            let _previous = reach.insert(start.clone(), members.clone());

            continue;
        }

        let mut seen: crate::HashSet<String> = crate::HashSet::default();
        let mut queue = vec![start.clone()];

        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }

            if let Some(neighbours) = edges.get(&name) {
                queue.extend(neighbours.iter().cloned());
            }
        }

        let _previous = reach.insert(start.clone(), seen);
    }

    reach
}

/// Works out which configuration predicates hold for each package of the workspace.
///
/// Asking `rustc` is one process for the whole run, and the feature closure is arithmetic over
/// metadata that has already been loaded, so this is cheap enough to do unconditionally.
///
/// A `rustc` that cannot be run leaves every set unconditional, which is exactly how the tool
/// behaved before it evaluated predicates at all: nothing is stripped, and a user on an unusual
/// toolchain gets a noisier report rather than a failed run.
fn configuration(metadata: &Metadata, args: &FeatureArgs) -> Cfgs {
    let Ok(target) = CfgSet::host() else {
        return Cfgs::unconditional();
    };

    Cfgs::new(&target, &features::enabled(metadata, args))
}

/// Loads cargo metadata for the tree at `dir`.
///
/// The feature selection has to match the one the build will use. Metadata decides which targets
/// exist and which files are walked, so discovering under one feature set and compiling under
/// another would place guards in files the compiler never sees.
pub(crate) fn load_metadata(dir: &Utf8Path, features: &FeatureArgs) -> Result<Metadata> {
    let mut command = MetadataCommand::new();

    let _builder = command.current_dir(dir).no_deps();

    if features.all_features {
        let _builder = command.features(CargoOpt::AllFeatures);
    }

    if features.no_default_features {
        let _builder = command.features(CargoOpt::NoDefaultFeatures);
    }

    if !features.features.is_empty() {
        // Cargo accepts a comma-separated list in one argument and repetition across several, so
        // the entries are split apart here and handed over as the flat list they denote.
        let named: Vec<String> = features
            .features
            .iter()
            .flat_map(|entry| entry.split([',', ' ']))
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        let _builder = command.features(CargoOpt::SomeFeatures(named));
    }

    command
        .exec()
        .map_err(|cause| error!("could not read cargo metadata for `{dir}`").caused_by(cause))
}

/// Returns the first `--package` name that no workspace member answers to.
///
/// A misspelled package name would otherwise select nothing and report a clean run over an empty
/// population, which reads exactly like a workspace with no gaps in its tests.
fn unknown_packages<'args>(metadata: &Metadata, args: &'args SelectArgs) -> Option<&'args str> {
    let known: crate::HashSet<&str> = metadata
        .workspace_packages()
        .iter()
        .map(|package| package.name.as_str())
        .collect();

    args.packages
        .iter()
        .map(String::as_str)
        .find(|wanted| !known.contains(wanted))
}

/// Returns whether a target contains code worth mutating.
///
/// Test, bench and example targets are excluded: mutating a test measures the tests' tests, and
/// mutating an example measures nothing at all, since examples are usually not run by the suite.
fn is_mutable_target(target: &Target) -> bool {
    target.kind.iter().any(|kind| {
        matches!(
            kind.to_string().as_str(),
            "lib" | "rlib" | "cdylib" | "proc-macro" | "bin"
        )
    })
}

/// Names every test target the workspace declares.
///
/// The name is what cargo reports as `target.name` for the binary it builds, and so is what a
/// `--include-test` or `--exclude-test` pattern is written against. Bench and example targets are
/// listed when they carry `test = true`, because cargo builds and runs those as test binaries too,
/// and a run that cannot name them cannot take them out of the oracle.
///
/// Names are deduplicated because two workspace members may each have a `tests/integration.rs`,
/// and a pattern naming it means both. That is the honest reading: these patterns select targets,
/// and `--test-package` is what selects by package.
fn test_targets(metadata: &Metadata) -> Vec<String> {
    let mut names: Vec<String> = metadata
        .workspace_packages()
        .iter()
        .flat_map(|package| package.targets.iter())
        .filter(|target| target.test)
        .map(|target| target.name.clone())
        .collect();

    names.sort();
    names.dedup();
    names
}

/// Returns the first `--file` or `--exclude-file` pattern that matches no source file.
///
/// A pattern that matches nothing is nearly always a mistake — a typo, a stale path after a move,
/// or separators written for the wrong platform. Left alone it produces an empty run that reports
/// no mutants and exits successfully, which reads in CI exactly like a clean bill of health. The
/// same reasoning already makes an unmatched `--ops` selector an error.
///
/// `walked` names every mutable source file in the workspace, not only the files this run selects,
/// so a workspace-wide pattern stays valid on a run narrowed with `--package`.
fn unmatched_pattern<'args>(walked: &[Utf8PathBuf], args: &'args SelectArgs) -> Option<&'args str> {
    args.files
        .iter()
        .chain(args.exclude_files.iter())
        .find(|pattern| !walked.iter().any(|path| matches_glob(pattern, path.as_str())))
        .map(String::as_str)
}

/// Returns whether a file passes the include and exclude patterns.
fn is_included(path: &Utf8Path, args: &SelectArgs) -> bool {
    let text = path.as_str();

    if args.exclude_files.iter().any(|pattern| matches_glob(pattern, text)) {
        return false;
    }

    if args.files.is_empty() {
        return true;
    }

    args.files.iter().any(|pattern| matches_glob(pattern, text))
}

/// Matches a path against a glob pattern supporting `*`, `**` and `?`.
///
/// A pattern with no separator matches against the file name alone, so `--file lexer.rs` does what
/// it looks like it does regardless of how deep the file is.
///
/// Both sides are normalised to `/` first. Paths are walked with the platform's own separator, so
/// on Windows they arrive with `\`, whereas patterns are written with `/` — they are typed on a
/// command line, checked into a config file and shared across platforms. Without normalisation
/// every pattern silently matches nothing there, and the run reports zero mutants and succeeds.
#[must_use]
pub fn matches_glob(pattern: &str, path: &str) -> bool {
    let pattern = normalize_separators(pattern);
    let path = normalize_separators(path);

    if pattern.contains('/') {
        glob_match(pattern.as_bytes(), path.as_bytes())
    } else {
        let name = path.rsplit('/').next().unwrap_or(&path);

        glob_match(pattern.as_bytes(), name.as_bytes())
    }
}

/// Rewrites a path or pattern so that `/` is the only separator.
///
/// Only done on Windows: a backslash is a legal character in a Unix file name, and rewriting it
/// there would make `--file "odd\name.rs"` match a file that does not exist.
fn normalize_separators(text: &str) -> String {
    if cfg!(windows) { text.replace('\\', "/") } else { text.to_owned() }
}

/// Backtracking glob matcher in which `*` stops at a separator and `**` does not.
///
/// This backtracks properly rather than keeping a single "most recent star" position. That
/// shortcut is the usual way to write a glob matcher and it is wrong as soon as separators are
/// significant: `src/**/*.rs` against `src/deep/nested/main.rs` needs the inner `*` to fail and
/// the outer `**` to then consume more, which a single star position cannot express.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let Some(&first) = pattern.first() else {
        return text.is_empty();
    };

    if first == b'*' {
        let stars = pattern.iter().take_while(|byte| **byte == b'*').count();
        let rest = &pattern[stars..];

        if stars > 1 {
            // `**` crosses separators, and `**/` may also match no directory at all.
            if let Some((b'/', after)) = rest.split_first() && glob_match(after, text) {
                return true;
            }

            return (0..=text.len()).any(|split| glob_match(rest, &text[split..]));
        }

        let mut split = 0;

        loop {
            if glob_match(rest, &text[split..]) {
                return true;
            }

            if split >= text.len() || text[split] == b'/' {
                return false;
            }

            split += 1;
        }
    }

    let Some((&head, tail)) = text.split_first() else {
        return false;
    };

    if first == b'?' {
        return head != b'/' && glob_match(&pattern[1..], tail);
    }

    head == first && glob_match(&pattern[1..], tail)
}

/// Lists every `.rs` file under a directory, in a deterministic order.
fn walk_rust_files(directory: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut found: Vec<Utf8PathBuf> = WalkDir::new(directory)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_type().is_dir())
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.into_path()).ok())
        .filter(|path| path.extension() == Some("rs"))
        .collect();

    found.sort();
    found
}

/// Assigns a mutant to a shard.
///
/// This uses jump consistent hashing rather than `hash % count`, because the two behave very
/// differently when the shard count changes. With a modulus, bumping a nightly job from 8 shards
/// to 9 reshuffles roughly 8/9 of all mutants into different shards; with jump consistent hashing
/// only the fraction that must move does. Shard membership is therefore something a team can
/// reason about across a config change instead of a fresh random assignment each time.
#[must_use]
pub fn shard_of(id: &str, count: u32) -> u32 {
    if count <= 1 {
        return 0;
    }

    let mut key = fnv1a(id.as_bytes());
    let mut candidate: i64 = -1;
    let mut next: i64 = 0;

    while next < i64::from(count) {
        candidate = next;
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);

        #[expect(clippy::cast_precision_loss, reason = "only the leading bits steer the choice")]
        let divisor = ((key >> 33).wrapping_add(1)) as f64;

        #[expect(clippy::cast_precision_loss, reason = "the operand is a small shard ordinal")]
        let scaled = ((candidate + 1) as f64) * (f64::from(1_u32 << 31) / divisor);

        #[expect(
            clippy::cast_possible_truncation,
            reason = "the value is bounded by the shard count"
        )]
        {
            next = scaled as i64;
        }
    }

    u32::try_from(candidate.max(0)).unwrap_or(0)
}

/// FNV-1a, used only to turn an id into a shard key.
const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;

    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }

    hash
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_bare_name_matches_at_any_depth() {
        assert!(matches_glob("lexer.rs", "src/parse/lexer.rs"));
        assert!(!matches_glob("lexer.rs", "src/parse/parser.rs"));
    }

    #[test]
    fn a_star_does_not_cross_separators() {
        assert!(matches_glob("src/*.rs", "src/main.rs"));
        assert!(!matches_glob("src/*.rs", "src/deep/main.rs"));
    }

    #[test]
    fn a_double_star_crosses_separators() {
        assert!(matches_glob("src/**/*.rs", "src/deep/nested/main.rs"));
        assert!(matches_glob("src/**", "src/deep/nested/main.rs"));
    }

    #[test]
    fn a_question_mark_matches_one_character() {
        assert!(matches_glob("a?.rs", "ab.rs"));
        assert!(!matches_glob("a?.rs", "abc.rs"));
    }

    #[test]
    fn an_exact_pattern_matches_exactly() {
        assert!(matches_glob("src/main.rs", "src/main.rs"));
        assert!(!matches_glob("src/main.rs", "src/other.rs"));
    }

    #[test]
    fn excludes_beat_includes() {
        let args = SelectArgs {
            files: vec!["src/**/*.rs".to_owned()],
            exclude_files: vec!["generated.rs".to_owned()],
            ..SelectArgs::default()
        };

        assert!(is_included(Utf8Path::new("src/lexer.rs"), &args));
        assert!(!is_included(Utf8Path::new("src/generated.rs"), &args));
    }

    #[test]
    fn no_include_patterns_means_everything() {
        assert!(is_included(Utf8Path::new("anything.rs"), &SelectArgs::default()));
    }

    #[test]
    fn one_shard_holds_everything() {
        for id in ["a", "b", "deadbeef1234"] {
            assert_eq!(shard_of(id, 1), 0);
        }
    }

    #[test]
    fn shards_are_always_in_range() {
        for count in 1_u32..=16 {
            for index in 0..500_u32 {
                let id = format!("mutant{index:04}");
                let shard = shard_of(&id, count);

                assert!(shard < count, "{id} landed in shard {shard} of {count}");
            }
        }
    }

    #[test]
    fn sharding_is_deterministic() {
        assert_eq!(shard_of("abc123def456", 7), shard_of("abc123def456", 7));
    }

    #[test]
    fn every_mutant_lands_in_exactly_one_shard() {
        let ids: Vec<String> = (0..300).map(|index| format!("mutant{index:04}")).collect();

        for count in [2_u32, 5, 7, 16] {
            let total: usize = (0..count)
                .map(|shard| ids.iter().filter(|id| shard_of(id, count) == shard).count())
                .sum();

            assert_eq!(total, ids.len(), "shard count {count} lost or duplicated mutants");
        }
    }

    #[test]
    fn shards_are_reasonably_balanced() {
        let ids: Vec<String> = (0..2000).map(|index| format!("mutant{index:05}")).collect();
        let count = 8_u32;
        let expected = ids.len() / count as usize;

        for shard in 0..count {
            let size = ids.iter().filter(|id| shard_of(id, count) == shard).count();

            assert!(
                size > expected / 2 && size < expected * 2,
                "shard {shard} holds {size}, expected around {expected}"
            );
        }
    }

    #[test]
    fn growing_the_shard_count_moves_few_mutants() {
        // The whole reason for jump consistent hashing: a team that raises its nightly shard count
        // should keep most of its coverage history, not reshuffle everything.
        let ids: Vec<String> = (0..2000).map(|index| format!("mutant{index:05}")).collect();
        let moved = ids.iter().filter(|id| shard_of(id, 8) != shard_of(id, 9)).count();
        let total = ids.len();

        // A modulus would move about 8/9 of them.
        assert!(moved < total / 4, "{moved} of {total} mutants moved when growing 8 -> 9");
    }

    #[test]
    fn different_ids_can_land_in_different_shards() {
        let ids: Vec<String> = (0..100).map(|index| format!("mutant{index:04}")).collect();
        let distinct: crate::HashSet<u32> = ids.iter().map(|id| shard_of(id, 4)).collect();

        assert!(distinct.len() > 1, "sharding put everything in one shard");
    }

    #[test]
    fn a_package_reaches_itself() {
        // Otherwise every mutant in a leaf crate would be reported as unreachable by its own tests.
        let metadata = load_metadata(Utf8Path::new(env!("CARGO_MANIFEST_DIR")), &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);

        for (package, reachable_from) in &reach {
            assert!(reachable_from.contains(package), "{package} does not reach itself");
        }
    }

    #[test]
    fn a_dependent_reaches_what_it_depends_on() {
        let metadata = load_metadata(Utf8Path::new(env!("CARGO_MANIFEST_DIR")), &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);

        // The binary crate is deliberately thin and defers everything to the library, so it must
        // reach it; the reverse must not hold, or the filter would never exclude anything here.
        let from_binary = reach.get("cargo-gamma").expect("the binary crate is a workspace member");

        assert!(from_binary.contains("cargo-gamma-lib"), "{from_binary:?}");

        let from_library = reach.get("cargo-gamma-lib").expect("the library is a workspace member");

        assert!(!from_library.contains("cargo-gamma"), "{from_library:?}");
    }

    #[test]
    fn a_package_that_depends_through_a_non_member_reaches_the_whole_workspace() {
        // Regression, issue-006. `cargo metadata --no-deps` does not list packages outside the
        // workspace, so an `app -> facade -> core` chain through a path dependency that is not a
        // member is invisible. Concluding "app does not reach core" from a graph with a hole in it
        // means never running app's tests against a mutant in core, and scoring that mutant
        // uncovered when a test does in fact cover it. Reaching everything is the fail-open answer.
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"core\", \"app\"]\nexclude = [\"facade\"]\nresolver = \"3\"\n",
        );
        write(
            &root,
            "core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        write(&root, "core/src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\n");

        // Outside the workspace on purpose: this is the package the metadata cannot see through.
        write(
            &root,
            "facade/Cargo.toml",
            "[package]\nname = \"facade\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\ncore = { path = \"../core\" }\n\n[workspace]\n",
        );
        write(&root, "facade/src/lib.rs", "pub use core::add;\n");

        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nfacade = { path = \"../facade\" }\n",
        );
        write(&root, "app/src/lib.rs", "pub fn go() -> i32 { facade::add(1, 2) }\n");

        let metadata = load_metadata(&root, &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);
        let from_app = reach.get("app").expect("app is a workspace member");

        assert!(from_app.contains("core"), "{from_app:?}");

        // The package with no such dependency keeps its exact reach: fail-open must not become
        // "everything reaches everything", which would run every binary for every mutant.
        let from_core = reach.get("core").expect("core is a workspace member");

        assert!(!from_core.contains("app"), "{from_core:?}");
    }

    #[test]
    fn a_registry_dependency_does_not_make_a_package_opaque() {
        // Regression, issue-006. Only a *path* dependency can lead back into the workspace. Marking
        // a package opaque for an ordinary crates.io dependency would make almost every real
        // workspace reach everything, undoing the scoping this whole graph exists for.
        let metadata = load_metadata(Utf8Path::new(env!("CARGO_MANIFEST_DIR")), &FeatureArgs::default()).expect("metadata");
        let reach = reachable(&metadata);
        let from_library = reach.get("cargo-gamma-lib").expect("the library is a workspace member");

        assert!(!from_library.contains("cargo-gamma"), "this crate has many registry dependencies: {from_library:?}");
    }

    #[test]
    fn a_pattern_and_a_path_are_compared_on_the_same_separators() {
        // Regression, issue-002. `walkdir` yields `src\a.rs` on Windows while every pattern anyone
        // writes uses `/`, so a `--file src/*.rs` matched nothing there and the run silently
        // examined no files at all.
        assert!(matches_glob("src/*.rs", &format!("src{}a.rs", std::path::MAIN_SEPARATOR)));
        assert!(matches_glob("src/**/*.rs", &format!("src{sep}deep{sep}a.rs", sep = std::path::MAIN_SEPARATOR)));

        // Forward slashes keep working on every platform, since that is what a pattern is written in.
        assert!(matches_glob("src/*.rs", "src/a.rs"));
    }

    /// Writes a two-package workspace and returns its root.
    ///
    /// `core` is a plain library, `app` is a binary that depends on it and also carries an example
    /// and an integration test — which is what makes it useful here, since those are exactly the
    /// target kinds the survey has to walk past.
    fn workspace() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"core\", \"app\"]\nresolver = \"3\"\n");

        write(
            &root,
            "core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n",
        );
        write(&root, "core/src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        write(&root, "core/src/generated.rs", "pub fn scale(x: i32) -> i32 {\n    x * 2\n}\n");

        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
        );
        write(&root, "app/src/main.rs", "fn main() {\n    let _ = 1 + 1;\n}\n");
        write(&root, "app/examples/demo.rs", "fn main() {\n    let _ = 2 + 2;\n}\n");
        write(&root, "app/tests/it.rs", "#[test]\nfn works() {\n    assert_eq!(1 + 1, 2);\n}\n");

        (directory, root)
    }

    /// A workspace whose shapes exercise the deduplication and graph-walking paths.
    ///
    /// `core` has both a library and a binary rooted in the same directory, so its files are
    /// walked twice; `mid` and `app` both depend on `core`, so the reachability walk meets it
    /// twice; and `core` reaches a module only through `#[cfg(test)]`.
    fn wide_workspace() -> (TempDir, Utf8PathBuf) {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");

        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"core\", \"mid\", \"app\"]\nresolver = \"3\"\n");

        write(
            &root,
            "core/Cargo.toml",
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"core-cli\"\npath = \"src/main.rs\"\n",
        );
        write(
            &root,
            "core/src/lib.rs",
            "#[cfg(test)]\nmod helpers;\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );
        write(&root, "core/src/main.rs", "fn main() {\n    let _ = 1 + 1;\n}\n");
        write(&root, "core/src/helpers.rs", "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n");

        write(
            &root,
            "mid/Cargo.toml",
            "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
        );
        write(&root, "mid/src/lib.rs", "pub fn triple(x: i32) -> i32 {\n    x * 3\n}\n");

        write(
            &root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncore = { path = \"../core\" }\nmid = { path = \"../mid\" }\n",
        );
        write(&root, "app/src/main.rs", "fn main() {\n    let _ = 2 + 2;\n}\n");

        (directory, root)
    }

    fn write(root: &Utf8Path, relative: &str, text: &str) {
        let path = root.join(relative);

        std::fs::create_dir_all(path.parent().expect("every fixture path has a parent").as_std_path())
            .expect("could not create the fixture directory");
        std::fs::write(path.as_std_path(), text).expect("could not write the fixture file");
    }

    fn survey(root: &Utf8Path, args: SelectArgs) -> Survey {
        Survey::new(&SelectArgs { dir: root.to_owned(), ..args }, None).expect("the fixture workspace must survey")
    }

    /// A file walked once per target still appears once, and test-only modules never appear.
    #[test]
    fn files_reached_twice_are_listed_once_and_test_only_modules_are_dropped() {
        let (_directory, root) = wide_workspace();
        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey.scan(None, &Selection::parse("@all").expect("selection"), &mut ordinals).expect("scan");
        let plan = survey.into_plan(scanned);

        let listed: Vec<&Utf8PathBuf> = plan.files.iter().map(|file| &file.path).collect();
        let core_lib = Utf8PathBuf::from("core/src/lib.rs");

        assert_eq!(listed.iter().filter(|path| ****path == core_lib).count(), 1, "{listed:?}");

        // `helpers.rs` is only ever reached through `#[cfg(test)] mod helpers;`, so it is test
        // code however ordinary it looks, and none of its mutants belong in the population.
        assert!(
            !plan.mutants.iter().any(|mutant| mutant.file.as_str().ends_with("helpers.rs")),
            "{:?}",
            plan.mutants.iter().map(|mutant| &mutant.file).collect::<Vec<_>>()
        );
    }

    /// A package two others depend on is visited once, however many paths lead to it.
    #[test]
    fn a_package_reached_by_two_paths_is_walked_once() {
        let (_directory, root) = wide_workspace();
        let survey = survey(&root, SelectArgs::default());
        let mut ordinals = 0;
        let scanned = survey.scan(None, &Selection::parse("@all").expect("selection"), &mut ordinals).expect("scan");
        let plan = survey.into_plan(scanned);

        // `app` reaches `core` directly and again through `mid`; the graph walk has to terminate
        // and report it once rather than loop or double-count.
        let reached = plan.reach.get("app").expect("app should have a reachable set");

        assert!(reached.contains("core"), "{reached:?}");
        assert!(reached.contains("mid"), "{reached:?}");
    }

    /// The names `--include-test` and `--exclude-test` are checked against.
    #[test]
    fn every_test_target_in_the_workspace_is_named() {
        let (_directory, root) = workspace();
        let survey = survey(&root, SelectArgs::default());

        // `it` is the integration target under `app/tests`, and the lib and bin targets are named
        // because their own unit tests build into binaries of the same name. `demo` is an example,
        // which cargo does not build as a test unless the manifest says so.
        assert!(survey.tests.contains(&"it".to_owned()), "{:?}", survey.tests);
        assert!(survey.tests.contains(&"core".to_owned()), "{:?}", survey.tests);
        assert!(survey.tests.contains(&"app".to_owned()), "{:?}", survey.tests);
        assert!(!survey.tests.contains(&"demo".to_owned()), "{:?}", survey.tests);
    }

    /// Test targets are collected across the whole workspace, since `--package` says what to
    /// mutate while these patterns say what judges it.
    #[test]
    fn test_targets_are_named_even_for_packages_left_unmutated() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["core".to_owned()],
            ..SelectArgs::default()
        };
        let survey = Survey::new(&args, None).expect("survey");

        assert!(survey.tests.contains(&"it".to_owned()), "{:?}", survey.tests);
    }

    #[test]
    fn a_package_that_is_not_in_the_workspace_is_a_usage_error() {
        // Silently surveying nothing would report a perfect score for a package name that was
        // simply mistyped, which is the worst possible way to learn about a typo.
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["nosuch".to_owned()],
            ..SelectArgs::default()
        };

        let error = Survey::new(&args, None).expect_err("an unknown package must not survey");

        assert!(error.to_string().contains("nosuch"), "{error}");
        assert!(error.is_usage(), "{error}");
    }

    #[test]
    fn naming_one_package_leaves_the_others_alone() {
        let (_directory, root) = workspace();
        let plan = survey(
            &root,
            SelectArgs {
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        assert!(plan.files.iter().all(|file| file.package == "core"), "{:?}", plan.files);
    }

    /// A workspace-wide exclusion must survive a run narrowed to one package.
    ///
    /// The patterns are written once, in `gamma.toml`, for the whole workspace, while `--package`
    /// narrows a single run. Checking them against only the narrowed files makes a correct config
    /// fail outright the moment someone runs a single package — which is what anyone iterating on
    /// one crate does, so the config that works in CI breaks on every local run.
    #[test]
    fn a_pattern_naming_another_package_is_still_matched_when_one_package_is_named() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["core".to_owned()],
            exclude_files: vec!["app/**".to_owned()],
            ..SelectArgs::default()
        };

        let plan = Survey::new(&args, None).expect("a pattern naming an unselected package must not be an error");

        assert!(plan.files.iter().all(|file| file.package == "core"), "{:?}", plan.files);
    }

    /// The wider walk must not weaken the check that catches a genuine typo.
    #[test]
    fn a_pattern_matching_nothing_in_the_whole_workspace_is_still_an_error() {
        let (_directory, root) = workspace();
        let args = SelectArgs {
            dir: root,
            packages: vec!["core".to_owned()],
            exclude_files: vec!["nosuch/**".to_owned()],
            ..SelectArgs::default()
        };

        let error = Survey::new(&args, None).expect_err("a pattern matching nothing must not survey");

        assert!(error.to_string().contains("nosuch/**"), "{error}");
        assert!(error.is_usage(), "{error}");
    }
    #[test]
    fn a_test_or_example_target_is_never_mutated() {
        // Mutating a test measures the tests' tests, and mutating an example measures nothing at
        // all, since the suite does not run examples.
        let (_directory, root) = workspace();
        let plan = survey(&root, SelectArgs::default());
        let paths: Vec<&str> = plan.files.iter().map(|file| file.path.as_str()).collect();

        assert!(!paths.iter().any(|path| path.contains("examples")), "{paths:?}");
        assert!(!paths.iter().any(|path| path.contains("tests")), "{paths:?}");
        assert!(paths.iter().any(|path| path.ends_with("main.rs")), "{paths:?}");
    }

    #[test]
    fn an_excluded_file_is_dropped_before_it_is_parsed() {
        let (_directory, root) = workspace();
        let plan = survey(
            &root,
            SelectArgs {
                exclude_files: vec!["**/generated.rs".to_owned()],
                ..SelectArgs::default()
            },
        );

        assert!(
            !plan.files.iter().any(|file| file.path.as_str().ends_with("generated.rs")),
            "{:?}",
            plan.files
        );
    }

    #[test]
    fn a_file_the_diff_never_mentions_is_dropped_before_it_is_parsed() {
        // This is most of what makes `--in-diff` affordable on a pull request: the files the change
        // did not touch are skipped without ever being read, let alone parsed.
        let (_directory, root) = workspace();

        write(&root, "change.patch", "--- a/core/src/lib.rs\n+++ b/core/src/lib.rs\n@@ -1,2 +1,2 @@\n one\n+    a + b\n");

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                ..SelectArgs::default()
            },
        );

        assert_eq!(plan.files.len(), 1, "{:?}", plan.files);
        assert!(plan.files[0].path.as_str().ends_with("lib.rs"), "{:?}", plan.files);
    }

    #[test]
    fn a_changed_file_still_only_yields_mutants_on_the_changed_lines() {
        // A changed line usually sits among many that were not touched. Mutating the whole file
        // would report on code the change never went near.
        let (_directory, root) = workspace();

        write(&root, "core/src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n");
        write(&root, "change.patch", "--- a/core/src/lib.rs\n+++ b/core/src/lib.rs\n@@ -1,2 +1,2 @@\n head\n+    a + b\n");

        let plan = survey(
            &root,
            SelectArgs {
                in_diff: Some(root.join("change.patch")),
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        let mut ordinals = 0;
        let scanned = plan
            .scan(None, &Selection::parse("all").expect("every mutator resolves"), &mut ordinals)
            .expect("the fixture must scan");

        assert!(!scanned.mutants.is_empty(), "the changed line yields nothing");
        assert!(scanned.mutants.iter().all(|mutant| mutant.line == 2), "{:?}", scanned.mutants);
    }

    #[test]
    fn a_mutant_an_earlier_report_settled_is_dropped_and_counted() {
        // `--iterate` exists so a second run costs only the mutants that were still open. A settled
        // mutant takes no ordinal and no shard slot, so it must be removed rather than re-run.
        let (_directory, root) = workspace();
        let mut plan = survey(
            &root,
            SelectArgs {
                packages: vec!["core".to_owned()],
                ..SelectArgs::default()
            },
        );

        let selection = Selection::parse("all").expect("every mutator resolves");
        let mut ordinals = 0;
        let first = plan.scan(None, &selection, &mut ordinals).expect("the fixture must scan");
        let settled: crate::HashSet<String> = first.mutants.iter().map(|mutant| mutant.id.clone()).collect();

        assert!(!settled.is_empty(), "the fixture yielded no mutants to settle");

        plan.settle(settled.clone());

        let mut ordinals = 0;
        let second = plan.scan(None, &selection, &mut ordinals).expect("the fixture must scan");

        assert!(second.mutants.is_empty(), "{:?}", second.mutants);
        assert_eq!(second.settled_out, settled.len());
    }

    #[test]
    fn a_feature_selection_is_carried_into_the_metadata_it_surveys() {
        // Discovering under one feature set and compiling under another would place guards in files
        // the compiler never sees, so every form of feature selection has to reach cargo.
        let (_directory, root) = workspace();

        for features in [
            FeatureArgs { all_features: true, ..FeatureArgs::default() },
            FeatureArgs { no_default_features: true, ..FeatureArgs::default() },
            FeatureArgs {
                features: vec!["core/extra, ".to_owned()],
                ..FeatureArgs::default()
            },
        ] {
            let metadata = load_metadata(&root, &features).expect("the fixture workspace must produce metadata");

            assert_eq!(metadata.workspace_packages().len(), 2, "{features:?}");
        }
    }

    #[test]
    fn metadata_that_cannot_be_read_names_the_directory() {
        let directory = TempDir::new().expect("a temporary directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is not UTF-8");
        let error = load_metadata(&root, &FeatureArgs::default()).expect_err("a directory with no manifest has no metadata");

        assert!(error.to_string().contains(root.as_str()), "{error}");
    }
}
