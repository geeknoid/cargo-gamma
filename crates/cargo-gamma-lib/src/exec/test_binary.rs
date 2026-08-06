use camino::{Utf8Path, Utf8PathBuf};
use core::time::Duration;
use serde_json::Value;
use std::collections::HashMap;

use crate::discover::{Plan, matches_glob};
use crate::model::{Mutant, Outcome};

use super::memory::MemoryPolicy;

/// A test executable and the package that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBinary {
    /// Where the executable is.
    pub path: Utf8PathBuf,

    /// The package it belongs to, which bounds the code it can possibly reach.
    pub package: String,

    /// The cargo target that produced it, which is what `--exclude-test` matches against.
    ///
    /// A package's unit tests take the name of the lib or bin they live in, and each file under
    /// `tests/` becomes a target of its own, so this is the finest granularity cargo offers for
    /// naming part of a suite — and the granularity `package` is too coarse for.
    pub target: String,

    /// The directory holding the package's `Cargo.toml`, which is where cargo would run it.
    ///
    /// `cargo test` sets the working directory to the package root, not the workspace root, so a
    /// test that opens `tests/data/fixture.json` or `src/../golden.txt` only finds it there. Running
    /// every binary from the workspace root makes those tests fail identically with and without a
    /// mutant active, which turns a whole package's worth of mutants into false survivors.
    pub manifest_dir: Utf8PathBuf,

    /// How long it took with no mutant active, so the cheapest can be tried first.
    pub baseline: Duration,

    /// This binary's share of a mutant's budget.
    pub budget: Duration,

    /// The peak memory the whole subtree reached with no mutant active, when it was measured.
    ///
    /// `None` means nobody measured it — the run did not ask, or the host could not — which is a
    /// different thing from a peak of zero and is why this is not a plain `u64`.
    pub peak: Option<u64>,

    /// The memory ceiling this binary's mutant runs are held to, when one applies.
    pub memory: Option<u64>,
}

/// Splits a mutant's budget across the binaries it may have to run.
///
/// The budget is calibrated from the baseline, which is the time the whole suite takes; applying it
/// to each binary in turn would silently hand a mutant that many times the budget it was promised,
/// and make any projection built from it wrong by the same factor. Each binary therefore gets the
/// share of the budget that matches its share of the baseline, so the parts add up to the whole.
///
/// The floor is *not* divided, because it is a different kind of quantity. The shares answer "how
/// long may this mutant run in total", and dividing them keeps that sum honest. The floor answers
/// "below what duration is a verdict meaningless", and that is a statement about measurement noise
/// on one binary, which does not get smaller because there are more binaries beside it. Dividing it
/// made it vanish on exactly the workspaces that need it most: a proportional share is the binary's
/// own baseline times the multiplier, so a suite of 240 binaries gave each a 20% margin, and 240
/// test processes contending for the same cores miss a 20% margin constantly. Those mutants were
/// then reported as timeouts, which count as caught — so the noise inflated the score and sent the
/// reader looking for hangs that were never there.
///
/// The floor is only ever spent when a binary is genuinely stuck, and the sweep stops at the first
/// binary that fails or times out, so the cost of being generous is one floor per killed mutant
/// rather than one per binary.
pub(super) fn apportion(binaries: &mut [TestBinary], timeout: Duration, floor: Duration) {
    let Ok(count) = u32::try_from(binaries.len()) else {
        return;
    };

    if count == 0 {
        return;
    }

    let total: Duration = binaries.iter().map(|binary| binary.baseline).sum();

    for binary in binaries.iter_mut() {
        // Without a baseline there is nothing to weigh them by, so they split it evenly.
        let portion = if total.is_zero() {
            timeout / count
        } else {
            timeout.mul_f64(binary.baseline.as_secs_f64() / total.as_secs_f64())
        };

        binary.budget = portion.max(floor);
    }
}

/// Derives each binary's memory ceiling from what the same binary used with no mutant active.
///
/// This belongs beside [`apportion`] because it is the other half of the same preparation: both
/// turn a baseline measurement into the budget a mutant of that binary is judged against, and both
/// have to happen after the baseline and before the first mutant runs.
///
/// The two are apportioned differently, though, and deliberately. A timeout is a quantity a run
/// spends once and divides between the binaries it has to visit, because a mutant's budget is the
/// time the whole sweep may take. Memory is not spent that way: the binaries run one after another,
/// each on its own, so each is bounded by what *it* was measured to need rather than by a share of
/// some total. Dividing a memory budget between binaries would bound a suite of two hundred test
/// targets two hundred times more tightly than a suite of one, which is a statement about the shape
/// of the workspace and not about any mutant in it.
///
/// `calibrated` says whether the baseline actually ran; see [`MemoryPolicy::ceiling`] for why a run
/// without one gets no derived ceiling at all.
pub(super) fn bound(binaries: &mut [TestBinary], policy: &MemoryPolicy, calibrated: bool) {
    for binary in binaries.iter_mut() {
        binary.memory = policy.ceiling(binary.peak, calibrated);
    }
}

/// Extracts the test executables cargo reported building.
pub(super) fn test_binaries(stdout: &str) -> Vec<TestBinary> {
    let mut binaries = Vec::new();

    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }

        let is_test = message
            .get("profile")
            .and_then(|profile| profile.get("test"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !is_test {
            continue;
        }

        if let Some(path) = message.get("executable").and_then(Value::as_str) {
            let package = message
                .get("package_id")
                .and_then(Value::as_str)
                .map_or_else(String::new, package_name);

            let target = message
                .get("target")
                .and_then(|target| target.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();

            let manifest_dir = message
                .get("manifest_path")
                .and_then(Value::as_str)
                .map(Utf8PathBuf::from)
                .and_then(|manifest| manifest.parent().map(Utf8Path::to_path_buf))
                .unwrap_or_default();

            binaries.push(TestBinary {
                path: Utf8PathBuf::from(path),
                package,
                target,
                manifest_dir,
                baseline: Duration::ZERO,
                budget: Duration::ZERO,
                peak: None,
                memory: None,
            });
        }
    }

    binaries.sort_by(|left, right| left.path.cmp(&right.path));
    binaries.dedup_by(|left, right| left.path == right.path);
    binaries
}

/// Drops the test binaries `--include-test` and `--exclude-test` say must not decide a verdict.
///
/// This has to run before the baseline, not after. `apportion` splits a mutant's timeout between
/// the binaries in proportion to their baselines, so a binary removed afterwards would leave every
/// remaining share computed from a suite that is not the one being run — each mutant would be given
/// a fraction of its budget and the surplus would simply go unspent, turning slow tests into
/// timeouts and timeouts into kills. Removing them here means the baseline never sees them and the
/// shares are honest.
pub(super) fn restrict(binaries: &mut Vec<TestBinary>, include: &[String], exclude: &[String]) {
    if include.is_empty() && exclude.is_empty() {
        return;
    }

    binaries.retain(|binary| admits_target(&binary.target, include, exclude));
}

/// Whether a target name survives the include and exclude patterns.
///
/// Exclusion is checked first and wins, which is what makes `--include-test "*"` plus a few
/// exclusions mean what it looks like. An empty include list admits everything, so exclusion alone
/// is a subtraction from the whole suite rather than a selection of nothing.
fn admits_target(name: &str, include: &[String], exclude: &[String]) -> bool {
    if exclude.iter().any(|pattern| matches_glob(pattern, name)) {
        return false;
    }

    include.is_empty() || include.iter().any(|pattern| matches_glob(pattern, name))
}

/// Returns the first `--include-test` or `--exclude-test` pattern that names no test target.
///
/// A pattern matching nothing is the failure these options exist to prevent. An `--exclude-test`
/// typo leaves the target it meant to remove in the oracle, so mutants that should have survived
/// are reported as caught and the score reads better than the suite deserves; an `--include-test`
/// typo empties the oracle instead. Neither says anything on its own, and both look in CI exactly
/// like a run that went well. The same reasoning already makes an unmatched `--file` an error.
pub(super) fn unmatched_test<'args>(tests: &[String], include: &'args [String], exclude: &'args [String]) -> Option<&'args str> {
    include
        .iter()
        .chain(exclude.iter())
        .find(|pattern| !tests.iter().any(|name| matches_glob(pattern, name)))
        .map(String::as_str)
}

/// Whether a test binary can possibly reach code in `package`.
///
/// An unknown package on either side means "assume it can": a missed optimization costs time, while
/// a wrong exclusion would report an untested mutant as unreachable and hide a real gap.
pub(super) fn reaches(binary: &TestBinary, package: &str, plan: &Plan, scope: &TestScope<'_>) -> bool {
    if !scope.admits(&binary.package) {
        return false;
    }

    if scope.whole_workspace {
        return true;
    }

    if binary.package.is_empty() || package.is_empty() {
        return true;
    }

    plan.reach.get(&binary.package).is_none_or(|reachable| reachable.contains(package))
}

/// Which packages need their test targets compiled at all.
///
/// A test binary is only ever run against a mutant its package can reach, so a package that cannot
/// reach anything being mutated produces binaries the run would build, baseline and then never
/// consult. Naming the useful subset lets cargo skip compiling the rest.
///
/// Returns `None` when the subset is the whole workspace, which is both the common case and the
/// one worth spelling as `--workspace`: cargo unifies features over the packages it is asked to
/// build, so narrowing the selection is a change in what gets compiled and not only in how much.
/// The caller is expected to fall back to the whole workspace if a narrowed build fails.
pub(super) fn build_packages(plan: &Plan, scope: &TestScope<'_>) -> Option<Vec<String>> {
    // Reach is keyed by every workspace member, so its keys are the population being narrowed from.
    // Without it there is nothing to compare a subset against, so there is no subset.
    if plan.reach.is_empty() {
        return None;
    }

    let mutated: crate::HashSet<&str> = plan
        .mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .map(|mutant| mutant.package.as_str())
        .collect();

    let mut wanted: Vec<String> = plan
        .reach
        .iter()
        .filter(|(package, reachable)| {
            if !scope.admits(package) {
                // A package whose own code is being mutated still has to compile its test targets:
                // a mutant can live in one of them.
                return mutated.contains(package.as_str());
            }

            scope.whole_workspace || reachable.iter().any(|name| mutated.contains(name.as_str()))
        })
        .map(|(package, _)| package.clone())
        .collect();

    if wanted.len() >= plan.reach.len() {
        return None;
    }

    // Deterministic, because it becomes a command line that is worth being able to compare between
    // runs.
    wanted.sort();

    Some(wanted)
}

/// Totals the work every live mutant represents, counting only the binaries that can reach it.
///
/// Returns the serial suite time and the serial budget: what testing each live mutant once would
/// cost if every reachable binary ran to completion, and what it would cost if every reachable
/// binary instead ran out its timeout. Both are summed per mutant rather than taken from the whole
/// suite, because that is what a run actually does — a mutant in a leaf crate never starts the
/// binaries that cannot link it.
pub(super) fn workload(mutants: &[Mutant], binaries: &[TestBinary], plan: &Plan, scope: &TestScope<'_>) -> (Duration, Duration) {
    let mut reachable = Duration::ZERO;
    let mut budget = Duration::ZERO;

    // Distinct packages are far fewer than mutants, so the reachable set is worked out once per
    // package and then multiplied, rather than re-derived for every mutant in a crate.
    let mut costs: HashMap<&str, (Duration, Duration)> = HashMap::new();

    for mutant in mutants.iter().filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending) {
        let (suite, worst) = *costs.entry(mutant.package.as_str()).or_insert_with(|| {
            binaries
                .iter()
                .filter(|binary| reaches(binary, &mutant.package, plan, scope))
                .fold((Duration::ZERO, Duration::ZERO), |(suite, worst), binary| (suite + binary.baseline, worst + binary.budget))
        });

        reachable += suite;
        budget += worst;
    }

    (reachable, budget)
}

/// Which test binaries a run is allowed to consult.
#[derive(Debug, Clone, Copy)]
pub(super) struct TestScope<'names> {
    /// Packages named by `--test-package`. Empty means no restriction.
    pub(super) packages: &'names [String],

    /// Whether every admitted binary runs for every mutant, reachable or not.
    pub(super) whole_workspace: bool,
}

impl TestScope<'_> {
    /// Returns whether a binary's package survives the `--test-package` filter.
    fn admits(&self, package: &str) -> bool {
        self.packages.is_empty() || self.packages.iter().any(|wanted| wanted == package)
    }
}

/// Extracts the package name from a cargo package id.
///
/// Two spellings are in circulation: the stable `path+file:///x/y#name@1.0.0` (name omitted when it
/// matches the last path segment) and the older `name 1.0.0 (source)`. An unrecognized id yields an
/// empty name, treated as "reaches everything" rather than "reaches nothing".
fn package_name(id: &str) -> String {
    if let Some((locator, fragment)) = id.rsplit_once('#') {
        return fragment.split_once('@').map_or_else(
            || {
                if is_version(fragment) {
                    // A bare version: the name is the last segment of the path before it.
                    locator.rsplit('/').next().unwrap_or_default().to_owned()
                } else {
                    fragment.to_owned()
                }
            },
            |(name, _version)| name.to_owned(),
        );
    }

    id.split_whitespace().next().unwrap_or_default().to_owned()
}

/// Whether a package id fragment is a bare version rather than a package name.
///
/// The release part of a version is digits and dots and nothing else, which no package name can be
/// mistaken for; anything after the first `-` or `+` is a pre-release or build tag and is ignored,
/// since those are made of the same letters a name is.
fn is_version(fragment: &str) -> bool {
    let release = fragment.split(['-', '+']).next().unwrap_or(fragment);

    !release.is_empty()
        && release.starts_with(|character: char| character.is_ascii_digit())
        && release.chars().all(|character| character.is_ascii_digit() || character == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope that admits every binary, which is what most of these tests are not about.
    const ANY: TestScope<'static> = TestScope { packages: &[], whole_workspace: false };

    #[test]
    fn test_binaries_are_read_from_cargo_json() {
        // Only test artifacts are runnable; the library artifact in the same stream is not.
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","profile":{"test":false},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/unit"}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error"}}"#,
            "\n",
            "not json at all",
            "\n",
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/cli"}"#,
            "\n",
        );

        // The list is sorted so that a run's per-binary ordering does not depend on cargo's
        // scheduling, which would make timings and any early exit irreproducible.
        let paths: Vec<Utf8PathBuf> = test_binaries(stdout).into_iter().map(|binary| binary.path).collect();

        assert_eq!(paths, vec![Utf8PathBuf::from("/tmp/cli"), Utf8PathBuf::from("/tmp/unit")]);
    }

    /// Nothing to apportion is not an error, and must not divide by the count either.
    #[test]
    fn apportioning_across_no_binaries_does_nothing() {
        let mut binaries: Vec<TestBinary> = Vec::new();

        apportion(&mut binaries, Duration::from_secs(10), Duration::from_secs(1));

        assert!(binaries.is_empty());
    }

    /// A whole-workspace build has no dependency graph to consult: every binary may reach anything.
    #[test]
    fn a_whole_workspace_scope_admits_every_binary_for_every_package() {
        let scope = TestScope {
            packages: &[],
            whole_workspace: true,
        };

        // `unrelated` is nowhere in the reachability graph, so only the whole-workspace answer can
        // be what admits it.
        assert!(reaches(&binary("unrelated"), "subject", &plan_reaching(&[]), &scope));
    }

    #[test]
    fn a_binary_carries_the_directory_of_its_packages_manifest() {
        let line = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/subject#0.1.0","manifest_path":"/w/crates/subject/Cargo.toml","profile":{"test":true},"executable":"/w/target/debug/deps/subject-abc"}"#;
        let found = test_binaries(line);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest_dir, "/w/crates/subject");
    }

    #[test]
    fn a_binary_cargo_did_not_locate_has_no_manifest_directory() {
        let line = r#"{"reason":"compiler-artifact","package_id":"path+file:///w#0.1.0","profile":{"test":true},"executable":"/w/target/debug/deps/subject-abc"}"#;
        let found = test_binaries(line);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest_dir, "");
    }

    fn binary(package: &str) -> TestBinary {
        TestBinary {
            package: package.to_owned(),
            ..crate::testing::test_binary("/tmp/t")
        }
    }

    /// The path has to differ, since `restrict` runs on a list `test_binaries` already deduplicated.
    fn target(name: &str) -> TestBinary {
        TestBinary {
            target: name.to_owned(),
            ..crate::testing::test_binary(&format!("/tmp/{name}"))
        }
    }

    fn names(binaries: &[TestBinary]) -> Vec<&str> {
        binaries.iter().map(|binary| binary.target.as_str()).collect()
    }

    #[test]
    fn the_target_name_is_kept_from_cargo_json() {
        let stdout =
            concat!(r#"{"reason":"compiler-artifact","profile":{"test":true},"target":{"name":"conformance_xsd"},"executable":"/tmp/c"}"#, "\n");

        let found = test_binaries(stdout);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, "conformance_xsd");
    }

    /// Cargo has always reported this, but an older or stubbed stream must not lose the binary.
    #[test]
    fn a_binary_with_no_target_name_is_still_kept() {
        let stdout = concat!(r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/c"}"#, "\n");

        let found = test_binaries(stdout);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, "");
    }

    #[test]
    fn no_patterns_leave_every_binary_in_place() {
        let mut binaries = vec![target("unit"), target("conformance_xsd")];

        restrict(&mut binaries, &[], &[]);

        assert_eq!(names(&binaries), vec!["unit", "conformance_xsd"]);
    }

    #[test]
    fn an_exclusion_glob_removes_the_targets_it_names() {
        let mut binaries = vec![target("unit"), target("conformance_xsd"), target("conformance_xpath")];

        restrict(&mut binaries, &[], &["conformance_*".to_owned()]);

        assert_eq!(names(&binaries), vec!["unit"]);
    }

    #[test]
    fn an_inclusion_glob_keeps_only_the_targets_it_names() {
        let mut binaries = vec![target("unit"), target("integration"), target("conformance_xsd")];

        restrict(&mut binaries, &["unit".to_owned(), "integration".to_owned()], &[]);

        assert_eq!(names(&binaries), vec!["unit", "integration"]);
    }

    /// Otherwise `--include-test "*"` with a few exclusions would quietly mean the whole suite.
    #[test]
    fn an_exclusion_beats_an_inclusion_that_also_matches() {
        let mut binaries = vec![target("unit"), target("conformance_xsd")];

        restrict(&mut binaries, &["*".to_owned()], &["conformance_*".to_owned()]);

        assert_eq!(names(&binaries), vec!["unit"]);
    }

    #[test]
    fn a_pattern_matching_a_declared_target_is_not_reported_as_unmatched() {
        let tests = vec!["unit".to_owned(), "conformance_xsd".to_owned()];

        assert_eq!(unmatched_test(&tests, &[], &["conformance_*".to_owned()]), None);
    }

    #[test]
    fn a_pattern_matching_nothing_is_reported() {
        let tests = vec!["unit".to_owned(), "conformance_xsd".to_owned()];
        let exclude = vec!["conformance_*".to_owned(), "confrmance_xpath".to_owned()];

        assert_eq!(unmatched_test(&tests, &[], &exclude), Some("confrmance_xpath"));
    }

    /// An inclusion typo empties the oracle instead of widening it, and is just as fatal.
    #[test]
    fn an_unmatched_inclusion_is_reported_too() {
        let tests = vec!["unit".to_owned()];

        assert_eq!(unmatched_test(&tests, &["untis".to_owned()], &[]), Some("untis"));
    }

    fn plan_reaching(edges: &[(&str, &[&str])]) -> Plan {
        let mut reach = crate::HashMap::default();

        for (from, to) in edges {
            let _previous =
                reach.insert((*from).to_owned(), to.iter().map(|name| (*name).to_owned()).collect());
        }

        Plan {
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach,
            specs: crate::HashMap::default(),
        }
    }

    /// A plan whose live mutants sit in `packages`, over the dependency graph `edges` describes.
    fn plan_mutating(edges: &[(&str, &[&str])], packages: &[&str]) -> Plan {
        let mut plan = plan_reaching(edges);

        plan.mutants = packages
            .iter()
            .enumerate()
            .map(|(index, package)| Mutant {
                id: (*package).to_owned(),
                ordinal: u32::try_from(index).unwrap_or(0).saturating_add(1),
                file: Utf8PathBuf::from("src/lib.rs"),
                package: (*package).to_owned(),
                span: 0..1,
                line: 1,
                column: 1,
                mutator: "arith.add_to_sub".to_owned(),
                item_path: "f".to_owned(),
                occurrence: 0,
                replacement_index: 0,
                original: "a + b".to_owned(),
                replacement: "a - b".to_owned(),
                shape: crate::ops::collect::Shape::Expr,
                outcome: Outcome::Pending,
                suppression: None,
                expectation: None,
                elapsed_ms: 0,
                killed_by: None,
                note: None,
            })
            .collect();

        plan
    }

    /// `app` and `tool` both link `core`; `aside` links nothing.
    fn graph() -> Vec<(&'static str, &'static [&'static str])> {
        vec![
            ("app", &["app", "core"] as &[&str]),
            ("tool", &["tool", "core"]),
            ("core", &["core"]),
            ("aside", &["aside"]),
        ]
    }

    #[test]
    fn a_package_whose_tests_can_reach_nothing_being_mutated_is_not_built() {
        let plan = plan_mutating(&graph(), &["core"]);

        // `aside` cannot link `core`, so every test binary it would produce is one the run would
        // compile, baseline and never consult.
        assert_eq!(build_packages(&plan, &ANY), Some(vec!["app".to_owned(), "core".to_owned(), "tool".to_owned()]));
    }

    #[test]
    fn mutating_everything_asks_for_the_whole_workspace() {
        let plan = plan_mutating(&graph(), &["core", "aside"]);

        assert_eq!(build_packages(&plan, &ANY), None, "a subset of everything is not a subset");
    }

    #[test]
    fn naming_the_tests_that_matter_narrows_the_build_to_them() {
        let plan = plan_mutating(&graph(), &["core"]);
        let named = [String::from("app")];
        let scope = TestScope {
            packages: &named,
            whole_workspace: false,
        };

        // Only `app`'s tests can return a verdict, so only `app`'s tests are worth compiling — and
        // `core` comes with them because a mutant can live in one of its own test targets.
        assert_eq!(build_packages(&plan, &scope), Some(vec!["app".to_owned(), "core".to_owned()]));
    }

    #[test]
    fn testing_the_whole_workspace_builds_the_whole_workspace() {
        let plan = plan_mutating(&graph(), &["core"]);
        let scope = TestScope {
            packages: &[],
            whole_workspace: true,
        };

        assert_eq!(build_packages(&plan, &scope), None);
    }

    #[test]
    fn a_workspace_with_no_dependency_graph_is_built_whole() {
        let plan = plan_mutating(&[], &["core"]);

        assert_eq!(build_packages(&plan, &ANY), None, "nothing is known, so nothing can be ruled out");
    }

    #[test]
    fn a_binary_only_reaches_what_its_package_links() {
        let plan = plan_reaching(&[("app", &["app", "core"]), ("core", &["core"])]);

        assert!(reaches(&binary("app"), "core", &plan, &ANY));
        assert!(reaches(&binary("app"), "app", &plan, &ANY));

        // The core crate does not link the app, so no test of it can reach the app's code.
        assert!(!reaches(&binary("core"), "app", &plan, &ANY));
    }

    #[test]
    fn an_unknown_package_reaches_everything() {
        let plan = plan_reaching(&[("app", &["app"])]);

        assert!(reaches(&binary(""), "core", &plan, &ANY), "an unattributed binary must not be skipped");
        assert!(reaches(&binary("app"), "", &plan, &ANY), "an unattributed mutant must not be skipped");
        assert!(reaches(&binary("stranger"), "core", &plan, &ANY), "a package we know nothing about");
    }

    #[test]
    fn a_test_package_filter_excludes_other_binaries() {
        let plan = plan_reaching(&[("app", &["app"]), ("other", &["other"])]);
        let named = [String::from("app")];
        let scope = TestScope {
            packages: &named,
            whole_workspace: false,
        };

        // Filtering by test package is a hard user request, so an otherwise reachable binary from
        // another package must not run.
        assert!(!reaches(&binary("other"), "other", &plan, &scope));
        assert!(reaches(&binary("app"), "app", &plan, &scope));
    }

    #[test]
    fn a_binary_is_attributed_to_the_package_that_produced_it() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/a","#,
            r#""package_id":"path+file:///w/crates/parser#cargo-gamma-lib@0.1.0"}"#,
            "\n",
        );

        assert_eq!(test_binaries(stdout)[0].package, "cargo-gamma-lib");
    }

    #[test]
    fn every_spelling_of_a_package_id_is_understood() {
        // The name is omitted when it matches the last path segment, which is the common case for
        // a workspace member and the one an over-eager parser gets wrong.
        assert_eq!(package_name("path+file:///w/crates/cargo-gamma-rt#0.1.0"), "cargo-gamma-rt");
        assert_eq!(package_name("path+file:///w/crates/parser#cargo-gamma-lib@0.1.0"), "cargo-gamma-lib");
        assert_eq!(package_name("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"), "serde");
        assert_eq!(package_name("serde 1.0.0 (registry+https://example.com)"), "serde");
    }

    #[test]
    fn a_pre_release_version_is_not_mistaken_for_a_package_name() {
        // The letters in `-beta` used to make this look like a name, which attributed the binary to
        // a package that does not exist and so quietly stopped it reaching anything.
        assert_eq!(package_name("path+file:///w/crates/cargo-gamma-rt#1.0.0-beta.1"), "cargo-gamma-rt");
        assert_eq!(package_name("path+file:///w/crates/cargo-gamma-rt#1.0.0+build7"), "cargo-gamma-rt");

        // A name that merely begins with a digit is still a name.
        assert_eq!(package_name("path+file:///w/crates/x#3d-tiles"), "3d-tiles");
    }

    #[test]
    fn a_budget_is_split_in_proportion_to_the_baseline() {
        let mut binaries = vec![binary("a"), binary("b")];

        binaries[0].baseline = Duration::from_secs(1);
        binaries[1].baseline = Duration::from_secs(3);

        apportion(&mut binaries, Duration::from_secs(40), Duration::ZERO);

        // The parts add up to the whole, so a mutant gets the budget it was promised and not one
        // multiplied by however many binaries happen to exist.
        assert_eq!(binaries[0].budget, Duration::from_secs(10));
        assert_eq!(binaries[1].budget, Duration::from_secs(30));
        assert_eq!(binaries[0].budget + binaries[1].budget, Duration::from_secs(40));
    }

    #[test]
    fn binaries_with_no_baseline_split_the_budget_evenly() {
        let mut binaries = vec![binary("a"), binary("b"), binary("c")];

        apportion(&mut binaries, Duration::from_secs(30), Duration::ZERO);

        for entry in &binaries {
            assert_eq!(entry.budget, Duration::from_secs(10));
        }
    }

    #[test]
    fn the_floor_is_a_promise_about_one_binary_not_about_the_sum() {
        let mut binaries = vec![binary("a"), binary("b")];

        binaries[0].baseline = Duration::from_millis(1);
        binaries[1].baseline = Duration::from_secs(100);

        apportion(&mut binaries, Duration::from_secs(20), Duration::from_secs(20));

        // A binary whose proportional share is a rounding error still cannot be judged in less
        // time than the floor says a verdict takes to be meaningful.
        assert_eq!(binaries[0].budget, Duration::from_secs(20));

        // The binary that earned a larger share keeps it.
        assert_eq!(binaries[1].budget, Duration::from_secs(20));
    }

    #[test]
    fn a_crowded_workspace_does_not_dilute_the_floor_away() {
        // This is the case that made the floor useless in practice. Two hundred binaries each get
        // a proportional share of about their own baseline, so a fast test's budget is a fraction
        // of a second; dividing the floor by the count left nothing to protect it, and a loaded
        // machine turned ordinary scheduling noise into timeouts that count as kills.
        let mut binaries: Vec<TestBinary> = (0..200).map(|_index| binary("a")).collect();

        for entry in &mut binaries {
            entry.baseline = Duration::from_millis(50);
        }

        apportion(&mut binaries, Duration::from_secs(12), Duration::from_secs(20));

        for entry in &binaries {
            assert_eq!(entry.budget, Duration::from_secs(20), "the floor was diluted again");
        }
    }

    #[test]
    fn a_binary_that_earns_more_than_the_floor_keeps_its_share() {
        // The floor raises a budget, it never caps one: a genuinely slow binary must still be
        // allowed the time its baseline says it needs.
        let mut binaries = vec![binary("a"), binary("b")];

        binaries[0].baseline = Duration::from_secs(100);
        binaries[1].baseline = Duration::from_secs(100);

        apportion(&mut binaries, Duration::from_secs(240), Duration::from_secs(20));

        assert_eq!(binaries[0].budget, Duration::from_secs(120));
    }

    #[test]
    fn an_unreadable_package_id_reaches_everything_rather_than_nothing() {
        // Guessing "nothing" would silently stop testing a mutant and report it as unreachable,
        // which reads as a finding about the code rather than a failure of this parser.
        assert_eq!(package_name(""), "");
    }
}
