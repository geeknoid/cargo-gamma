//! End-to-end mutation sessions against real crates.
//!
//! These tests drive the whole pipeline: copy the tree, vendor the guard runtime, build one
//! instrumented binary, measure the baseline and run every mutant. They invoke a real `cargo`, so
//! they are slower than the rest of the suite, but they are the only coverage that proves the
//! encoding in `schema.rs` actually compiles and that a verdict means what it claims.

mod common;

use camino::Utf8PathBuf;
use cargo_gamma_lib::run;
use common::FakeHost;
use std::fs;
use tempfile::TempDir;

/// Exit code for a run in which every gate passed.
const EXIT_OK: i32 = 0;

/// Exit code for a usage error, which is what a rejected option produces.
const EXIT_USAGE: i32 = 1;

/// A subject whose comparison is asserted exactly and whose side effect is not.
const SUBJECT: &str = "
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[derive(Default)]
pub struct Log {
    entries: Vec<u32>,
}

impl Log {
    pub fn record(&mut self, value: u32) {
        self.entries.push(value);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_pinned() {
        assert!(!is_adult(17));
        assert!(is_adult(18));
        assert!(is_adult(19));
    }

    #[test]
    fn recording_does_not_panic() {
        let mut log = Log::default();

        log.record(1);
    }
}
";

/// A crate whose only mutant cannot compile, so `suppress --eligible unviable` has something to write.
const UNVIABLE: &str = "
pub struct Marker;

pub fn lookup() -> Option<&'static Marker> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_builds() {
        assert!(super::lookup().is_none());
    }
}
";

/// Builds a throwaway crate containing `source`.
fn workspace(source: &str) -> TempDir {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");

    fs::create_dir_all(root.join("src")).expect("could not create src");
    fs::write(root.join("src/lib.rs"), source).expect("could not write the library");

    dir
}

/// Builds a crate whose second module is behind a feature that is off by default.
///
/// The module holds a mutant of a kind that could never compile if it were built at all, so a run
/// that reports it as anything other than unbuilt has either compiled code it should not have or
/// judged code no compiler ever read.
fn conditional() -> TempDir {
    let dir = workspace(SUBJECT);
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");

    fs::write(
        root.join("src/lib.rs"),
        format!("{SUBJECT}\n#[cfg(feature = \"extra\")]\npub mod extra;\n"),
    )
    .expect("could not write the library");

    fs::write(
        root.join("src/extra.rs"),
        "pub fn width(name: &String) -> usize {\n    name.len()\n}\n",
    )
    .expect("could not write the conditional module");

    dir
}

/// Builds a two-package workspace in which nothing links `island`.
///
/// `island` opts its lib target out of testing, so the build produces no test binary for it, and
/// `mainland` does not depend on it. No test that exists can reach the island's code.
fn archipelago() -> TempDir {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mainland\", \"island\"]\nresolver = \"2\"\n",
    )
    .expect("could not write the workspace manifest");

    for (name, extra) in [("mainland", ""), ("island", "\n[lib]\ntest = false\n")] {
        let package = root.join(name);

        fs::create_dir_all(package.join("src")).expect("could not create src");
        fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{extra}"),
        )
        .expect("could not write the manifest");
        fs::write(package.join("src/lib.rs"), SUBJECT).expect("could not write the library");
    }

    dir
}

/// Whether these tests are themselves running inside a mutation run.
///
/// Each of these drives a real cargo build. Nested inside a scratch tree that has already been
/// instrumented, that build fails for reasons that have nothing to do with any mutant, which turns
/// into a red baseline and stops the run before it starts. `CARGO_GAMMA` is set on every test
/// process precisely so a suite that shells out to cargo can step aside.
fn nested() -> bool {
    std::env::var_os("CARGO_GAMMA").is_some()
}

/// Runs a session against `dir` and returns the exit code and everything the tool printed.
fn session(dir: &TempDir, args: &[&str]) -> (i32, String) {
    let (code, out, err) = session_on(FakeHost::piped(), dir, args);

    (code, format!("{out}{err}"))
}

/// Runs a session on a given host, keeping the two streams apart.
fn session_on(mut host: FakeHost, dir: &TempDir, args: &[&str]) -> (i32, String, String) {
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut command = vec!["cargo-gamma".to_owned(), "gamma".to_owned(), "run".to_owned()];

    command.extend(args.iter().map(|arg| (*arg).to_owned()));
    command.push("--dir".to_owned());
    command.push(path.to_string());

    let code = run(&mut host, command);

    (code, host.stdout(), host.stderr())
}

#[test]
fn an_asserted_boundary_catches_its_mutant() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // `>=` becoming `>` moves the boundary onto 18, which the test pins, so it must be caught.
    assert!(!output.contains("MISSED src/lib.rs:3:5"), "{output}");
    assert!(output.contains("0 missed,"), "{output}");
}

#[test]
fn an_unasserted_side_effect_leaves_a_survivor() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--ops", "stmt"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // Nothing observes the recorded entry, so deleting the push cannot fail a test.
    assert!(output.contains("MISSED"), "{output}");
    assert!(output.contains("self.entries.push"), "{output}");
}

#[test]
fn a_failing_score_gate_fails_the_run() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--ops", "stmt", "--min-score", "100"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("below the required"), "{output}");
}

#[test]
fn a_suppressed_mutant_is_never_run() {
    if nested() {
        return;
    }
    let source = SUBJECT.replace(
        "pub fn is_adult(age: u32) -> bool {",
        "// #[gamma::skip(relational)]\npub fn is_adult(age: u32) -> bool {",
    );
    let dir = workspace(&source);
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("none tested, 2 suppressed"), "{output}");

    // Nothing was left to run, so the session must not have paid for a build at all.
    assert!(!output.contains("built once"), "{output}");
}

#[test]
fn a_red_baseline_is_reported_rather_than_measured() {
    if nested() {
        return;
    }
    let source = format!("{SUBJECT}\n#[test]\nfn always_fails() {{ panic!(\"nope\"); }}\n");
    let dir = workspace(&source);
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("baseline is not green"), "{output}");
}

#[test]
fn a_file_whose_every_mutant_is_unviable_still_converges() {
    if nested() {
        return;
    }
    // `Some(Default::default())` only compiles when the type implements `Default`, and `Marker`
    // does not, so the mutant has to be withdrawn. Withdrawing the only mutant in a file used to
    // leave the previous round's instrumented copy in the tree, so the offending guard survived
    // its own withdrawal and the build could never be made to succeed.
    let dir = workspace(UNVIABLE);
    let (code, output) = session(&dir, &["--ops", "fn_value.some_default", "--unviable"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // The summary line no longer counts unviable mutants, so the listing `--unviable` opts into is
    // what proves the withdrawal happened rather than the build failing outright.
    assert!(output.contains("[fn_value.some_default]"), "{output}");
    assert!(output.contains("none tested"), "{output}");
}

#[test]
fn the_reporters_write_a_conformant_document_and_a_self_contained_page() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let json = dir.path().join("report.json");
    let html = dir.path().join("report.html");
    let (code, output) = session(
        &dir,
        &[
            "--ops",
            "relational",
            "--json-report",
            json.to_str().expect("path is not UTF-8"),
            "--html",
            html.to_str().expect("path is not UTF-8"),
        ],
    );

    assert_eq!(code, EXIT_OK, "{output}");

    let document: serde_json::Value = serde_json::from_str(&fs::read_to_string(&json).expect("the JSON report was not written"))
        .expect("the JSON report is not valid JSON");

    assert_eq!(document["schemaVersion"], "2");
    assert_eq!(document["framework"]["name"], "cargo-gamma");

    let files = document["files"].as_object().expect("files is an object");
    let mutants = files["src/lib.rs"]["mutants"].as_array().expect("mutants is an array");

    assert!(!mutants.is_empty(), "the report has no mutants");
    assert!(files["src/lib.rs"]["source"].as_str().is_some_and(|s| s.contains("is_adult")));

    // The page has to survive being opened from a file:// URL with no network, so the viewer and
    // the payload both have to be in it, and nothing may be fetched.
    let page = fs::read_to_string(&html).expect("the HTML report was not written");

    assert!(page.contains("<mutation-test-report-app"), "the custom element is missing");
    assert!(!page.contains("cdn.jsdelivr.net"), "the offline page references a CDN");
    assert!(page.len() > 200_000, "the viewer was not inlined: {} bytes", page.len());
}

#[test]
fn the_config_file_is_honoured_by_a_real_run() {
    if nested() {
        return;
    }
    // The unit tests prove the merge; this proves the merged values actually reach the session,
    // which is the part that silently does nothing if the wiring is wrong.
    let dir = workspace(SUBJECT);

    fs::create_dir_all(dir.path().join(".cargo")).expect("could not create .cargo");
    fs::write(dir.path().join(".cargo/gamma.toml"), "ops = [\"stmt\"]\nmin-score = 100.0\n").expect("could not write the config");

    let (code, output) = session(&dir, &[]);

    // `stmt` leaves a survivor and the configured gate demands a perfect score, so a run that
    // ignored the file would pass with the default operator set and no gate at all.
    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("below the required"), "{output}");
}

#[test]
fn a_misspelled_config_key_stops_the_run() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    fs::create_dir_all(dir.path().join(".cargo")).expect("could not create .cargo");
    fs::write(dir.path().join(".cargo/gamma.toml"), "op = [\"stmt\"]\n").expect("could not write the config");

    let (code, output) = session(&dir, &[]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("unknown field"), "{output}");
}

#[test]
fn a_cargo_mutants_config_is_reported_as_unread() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    fs::create_dir_all(dir.path().join(".cargo")).expect("could not create .cargo");
    fs::write(dir.path().join(".cargo/mutants.toml"), "exclude_re = [\"impl Debug\"]\n").expect("could not write the foreign config");

    let (code, output) = session(&dir, &["--dry-run"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains(".cargo/mutants.toml is not read"), "{output}");
}

#[test]
fn migrating_writes_a_config_the_tool_then_honours() {
    if nested() {
        return;
    }
    // The unit tests prove the translation; this proves the file it writes is one a real run will
    // actually load, which is the only thing that makes the migration worth running.
    let dir = workspace(SUBJECT);

    fs::create_dir_all(dir.path().join(".cargo")).expect("could not create .cargo");
    fs::write(
        dir.path().join(".cargo/mutants.toml"),
        "exclude_globs = [\"src/lib.rs\"]\nexclude_re = [\"replace .* with Default::default\"]\nsome_future_key = 1\n",
    )
    .expect("could not write the foreign config");

    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "migrate".to_owned(),
            "-d".to_owned(),
            path.to_string(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}{}", host.stdout(), host.stderr());
    assert!(host.stderr().contains("left as TODO"), "{}", host.stderr());

    // Every file is now excluded, so a run under the generated config finds nothing and says so
    // rather than quietly reporting a perfect score over an empty set.
    let (code, output) = session(&dir, &["--dry-run"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("no mutants were generated"), "{output}");
}

#[test]
fn suppressing_writes_a_directive_that_actually_suppresses_the_mutant() {
    if nested() {
        return;
    }
    let dir = workspace(UNVIABLE);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "suppress".to_owned(),
            "--eligible".to_owned(),
            "unviable".to_owned(),
            "--ops".to_owned(),
            "fn_value.some_default".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{output}");

    let source = fs::read_to_string(dir.path().join("src/lib.rs")).expect("could not read the source");

    assert!(source.contains("// #[gamma::skip(fn_value.some_default"), "{source}");
    assert!(
        source.contains("written by cargo gamma suppress"),
        "the directive must say who wrote it"
    );

    // The written directive has to be one the tool itself honours; verification inside `suppress`
    // asserts that, and this asserts the verification was not vacuous.
    let (code, output) = session(&dir, &["--ops", "fn_value.some_default", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("none tested, 1 suppressed"), "{output}");
}

#[test]
fn suppressing_a_dry_run_prints_a_diff_and_changes_nothing() {
    if nested() {
        return;
    }
    let dir = workspace(UNVIABLE);
    let before = fs::read_to_string(dir.path().join("src/lib.rs")).expect("could not read the source");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "suppress".to_owned(),
            "--dry-run-suppress".to_owned(),
            "--eligible".to_owned(),
            "unviable".to_owned(),
            "--ops".to_owned(),
            "fn_value.some_default".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}{}", host.stdout(), host.stderr());
    assert!(host.stdout().contains('+'), "{}", host.stdout());
    assert!(host.stdout().contains("gamma::skip"), "{}", host.stdout());

    let after = fs::read_to_string(dir.path().join("src/lib.rs")).expect("could not read the source");

    assert_eq!(before, after, "a dry run must not touch the source");
}

#[test]
fn suppressing_refuses_to_touch_a_survivor() {
    if nested() {
        return;
    }
    // The guarantee the whole feature rests on, asserted through the CLI rather than the parser,
    // because the parser is not what a user reaches for.
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "suppress".to_owned(),
            "--eligible".to_owned(),
            "missed".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    assert_ne!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stderr().contains("gap in the test suite"), "{}", host.stderr());
}

#[test]
fn two_shards_merge_into_one_score() {
    if nested() {
        return;
    }

    // The end-to-end justification for sharding: two nights of partial runs have to add up to one
    // answer, or the feature only halves the work without ever producing a number.
    let dir = workspace(SUBJECT);
    let mut reports = Vec::new();

    for index in 0..2 {
        let path = dir.path().join(format!("shard-{index}.json"));
        let (code, output) = session(
            &dir,
            &[
                "--ops",
                "relational",
                "--shard-count",
                "2",
                "--shard-index",
                &index.to_string(),
                "--json-report",
                path.to_str().expect("path is not UTF-8"),
            ],
        );

        assert_eq!(code, EXIT_OK, "{output}");
        reports.push(path);
    }

    let merged = dir.path().join("merged.json");
    let mut host = FakeHost::piped();
    let mut command = vec!["cargo-gamma".to_owned(), "gamma".to_owned(), "merge".to_owned()];

    for path in &reports {
        command.push(path.to_str().expect("path is not UTF-8").to_owned());
    }

    command.push("--json-report".to_owned());
    command.push(merged.to_str().expect("path is not UTF-8").to_owned());

    let code = run(&mut host, command);
    let output = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("2 of 2 shards seen"), "{output}");
    assert!(output.contains("never tested"), "{output}");

    // The merged population must be the union, not either half.
    let document: serde_json::Value = serde_json::from_str(&fs::read_to_string(&merged).expect("the merged report was not written"))
        .expect("the merged report is not valid JSON");

    let count = document["files"]["src/lib.rs"]["mutants"]
        .as_array()
        .expect("mutants is an array")
        .len();

    let (code, single) = session(&dir, &["--ops", "relational", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{single}");

    let whole = single
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("Summary "))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|count| count.parse::<usize>().ok())
        .expect("the plan reports how many mutants it found");

    assert_eq!(count, whole, "the merged population must be the whole population");
}

#[test]
fn a_merged_score_gate_can_fail_the_build() {
    if nested() {
        return;
    }

    let dir = workspace(SUBJECT);
    let path = dir.path().join("shard.json");
    let (code, output) = session(&dir, &["--ops", "stmt", "--json-report", path.to_str().expect("path is not UTF-8")]);

    assert_eq!(code, EXIT_OK, "{output}");

    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "merge".to_owned(),
            path.to_str().expect("path is not UTF-8").to_owned(),
            "--min-score".to_owned(),
            "100".to_owned(),
        ],
    );

    assert_ne!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stderr().contains("merged mutation score"), "{}", host.stderr());
}

#[test]
fn estimating_projects_the_rest_of_the_run_and_then_runs_it() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--estimate".to_owned(),
            "--ops".to_owned(),
            "relational".to_owned(),
            // Phase lines belong to the progress display, so asserting on them needs it on.
            "--progress".to_owned(),
            "always".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{output}");

    // The projection rests on the fixed cost, so it cannot be printed before that is paid.
    assert!(output.contains("Baseline"), "{output}");
    assert!(output.contains("Estimate"), "{output}");
    assert!(output.contains("worst case"), "{output}");

    // One line, not a block: the build and baseline it would otherwise repeat are on the screen
    // immediately above it.
    let estimate = output
        .lines()
        .find(|line| line.contains("Estimate"))
        .expect("the estimate line is missing");

    assert!(estimate.contains("worst case"), "the estimate must fit one line: {estimate}");

    // And unlike the subcommand it replaced, it carries on and actually tests the mutants.
    assert!(output.contains("Summary"), "{output}");
    assert!(output.contains("2 mutants ("), "{output}");
}

#[test]
fn no_estimate_is_printed_unless_it_was_asked_for() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--ops".to_owned(),
            "relational".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("Estimate"), "{output}");
}

#[test]
fn the_estimate_survives_being_piped() {
    // It is the one line the user explicitly asked for, so suppressing it along with the progress
    // display when stdout is not a terminal would defeat the flag in exactly the setting — a CI
    // log — where knowing the remaining cost matters most.
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--estimate".to_owned(),
            "--ops".to_owned(),
            "relational".to_owned(),
            "--progress".to_owned(),
            "never".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("Estimate"), "{output}");
}

/// Builds a crate whose only oracle for the boundary lives in a named integration target.
///
/// The library carries no unit tests at all, so `tests/pinned.rs` is the one thing that can convict
/// the relational mutant. Taking that target out of the oracle has to turn a caught mutant into a
/// survivor, which is what makes the effect of the option observable rather than asserted about
/// its own plumbing.
fn split_oracle() -> TempDir {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let root = dir.path();

    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .expect("could not write the manifest");

    fs::create_dir_all(root.join("src")).expect("could not create src");
    fs::write(root.join("src/lib.rs"), "pub fn is_adult(age: u32) -> bool {\n    age >= 18\n}\n").expect("could not write the library");

    fs::create_dir_all(root.join("tests")).expect("could not create tests");
    fs::write(
        root.join("tests/pinned.rs"),
        "#[test]\nfn the_boundary_is_pinned() {\n    assert!(!subject::is_adult(17));\n    assert!(subject::is_adult(18));\n}\n",
    )
    .expect("could not write the integration test");

    dir
}

#[test]
fn excluding_a_test_target_takes_it_out_of_the_oracle() {
    if nested() {
        return;
    }
    let dir = split_oracle();

    // The control: the integration target is present, so the boundary is pinned and nothing gets
    // past it. Without this half, the assertion below would also pass on a crate that never had a
    // working oracle in the first place.
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("0 missed,"), "{output}");

    let (code, output) = session(&dir, &["--ops", "relational", "--exclude-test", "pinned"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("MISSED"), "{output}");
    assert!(output.contains("1 test target not consulted"), "{output}");
}

#[test]
fn including_only_the_unit_tests_leaves_the_integration_target_out() {
    if nested() {
        return;
    }
    let dir = split_oracle();
    let (code, output) = session(&dir, &["--ops", "relational", "--include-test", "subject"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("MISSED"), "{output}");
}

#[test]
fn a_test_pattern_that_names_no_target_stops_the_run() {
    if nested() {
        return;
    }
    let dir = split_oracle();

    // A misspelled exclusion silently widens the oracle, so mutants the missing target would have
    // let through are reported as caught and the score reads better than the suite deserves. That
    // is indistinguishable in CI from a run that went well, which is why it is fatal.
    let (code, output) = session(&dir, &["--ops", "relational", "--exclude-test", "pinnd"]);

    assert_eq!(code, EXIT_USAGE, "{output}");
    assert!(output.contains("pinnd"), "{output}");
}

#[test]
fn the_advise_surface_that_became_a_flag_is_gone() {
    // `advise` was a run that also diagnosed, and `--yields` was half of that diagnosis. Both are
    // now `--advice <PATH>`, so the analysis is spelled once and lands somewhere it can be shared.
    for argv in [
        vec!["run".to_owned(), "--advise".to_owned()],
        vec!["run".to_owned(), "--yields".to_owned()],
        vec!["advise".to_owned()],
    ] {
        let mut host = FakeHost::piped();
        let mut full = vec!["cargo-gamma".to_owned(), "gamma".to_owned()];

        full.extend(argv.clone());

        assert_eq!(run(&mut host, full), EXIT_USAGE, "{argv:?} was accepted");
    }
}

#[test]
fn advice_is_written_as_markdown_and_carries_the_family_table() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let advice = path.join("advice.md");
    let mut host = FakeHost::piped();
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--advice".to_owned(),
            advice.to_string(),
            "--ops".to_owned(),
            "relational,stmt".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    let output = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{output}");

    let text = fs::read_to_string(&advice).expect("the advice file was not written");

    assert!(text.starts_with("# Mutation testing advice"), "{text}");
    assert!(text.contains("## Contents"), "{text}");
    assert!(text.contains("## This run"), "{text}");
    assert!(text.contains("| Family |"), "{text}");
    assert!(text.contains("`relational`"), "{text}");

    // Every entry in the table of contents must land on a heading that is actually in the file.
    // A table of contents whose links do not resolve is worse than none, because it is only found
    // to be broken by someone who already had to scroll.
    let slug = |heading: &str| -> String {
        heading
            .chars()
            .filter(|character| character.is_alphanumeric() || *character == ' ' || *character == '-')
            .map(|character| if character == ' ' { '-' } else { character.to_ascii_lowercase() })
            .collect()
    };

    let headings: Vec<String> = text
        .lines()
        .filter_map(|line| line.strip_prefix("## ").or_else(|| line.strip_prefix("### ")))
        .map(slug)
        .collect();

    for line in text.lines().filter(|line| line.trim_start().starts_with("- [")) {
        let anchor = line
            .split("](#")
            .nth(1)
            .expect("a contents entry links somewhere")
            .trim_end_matches(')');

        assert!(
            headings.contains(&anchor.to_owned()),
            "the contents entry `{anchor}` has no heading in:\n{text}"
        );
    }

    // A tiny healthy crate must not be told its two files are each half the population.
    assert!(!text.contains("hot-file"), "{text}");

    // The diagnosis is a document now, so it must not also be dumped on the console.
    assert!(!output.contains("survivors/cpu-h"), "{output}");
}

#[test]
fn the_job_summary_carries_the_advice() {
    // The summary panel is the artifact a team reads every morning; a score with no diagnosis
    // beside it is the reason a nightly run gets ignored.
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let summary = path.join("summary.md");
    let mut host = FakeHost::piped().with_env("GITHUB_STEP_SUMMARY", summary.as_str());
    let code = run(
        &mut host,
        vec![
            "cargo-gamma".to_owned(),
            "gamma".to_owned(),
            "run".to_owned(),
            "--annotations".to_owned(),
            "github".to_owned(),
            "--ops".to_owned(),
            "relational".to_owned(),
            "--dir".to_owned(),
            path.to_string(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}", host.stderr());

    let text = fs::read_to_string(&summary).expect("the job summary was not written");

    assert!(text.contains("## Mutation testing"), "{text}");
    assert!(text.contains("### Findings"), "{text}");
    assert!(text.contains("| Family |"), "{text}");

    // The panel owns the heading and has just stated the score, so the fragment must not open a
    // level-one title beneath it or repeat the verdict table above it.
    assert!(!text.contains("# Mutation testing advice"), "{text}");
    assert!(!text.contains("## This run"), "{text}");
}

/// A crate whose tests print far more than a pipe will hold.
///
/// A pipe is about 64 KB. Before the output was drained concurrently, a binary like this blocked
/// forever in `write` while the run waited for it to exit — which the baseline reported as a
/// ten-minute stall, and which a mutant would have been recorded as a timeout for. A timeout counts
/// as detected, so a chatty test could silently turn a survivor into a passing score.
const CHATTY: &str = "
pub fn is_adult(age: u32) -> bool {
    age >= 18
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_is_pinned_loudly() {
        for index in 0..40_000 {
            println!(\"line {index} of a test that has a great deal to say about nothing at all\");
        }

        assert!(!is_adult(17));
        assert!(is_adult(18));
    }
}
";

#[test]
fn a_test_that_outprints_the_pipe_does_not_deadlock() {
    if nested() {
        return;
    }
    let dir = workspace(CHATTY);
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // Both mutants break the assertion, so libtest dumps the captured output of a failing test —
    // megabytes of it — down the pipe. The verdict has to be the real one, reached promptly, rather
    // than the timeout a blocked pipe would have produced once the budget expired.
    assert!(
        output.contains("2 mutants (2 caught, 0 missed, 0 timed out, 0 out of memory, 0 uncovered => 100.0%)"),
        "{output}"
    );
}

/// A crate whose mutant makes a loop run forever.
///
/// `drain` terminates only because the condition eventually goes false. Relaxing `>` to `>=` makes
/// it true for every `u64`, and because the body saturates rather than overflowing, the loop spins
/// instead of panicking. That is precisely the mutant the stall detector exists for: the process
/// stays alive and busy while producing no output at all, so nothing but silence distinguishes it
/// from a test that is merely slow.
const HANGS: &str = "
pub fn drain(mut remaining: u64) -> u64 {
    let mut steps = 0_u64;

    while remaining > 0 {
        remaining = remaining.saturating_sub(1);
        steps = steps.wrapping_add(1);
    }

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_terminates() {
        assert_eq!(drain(3), 3);
    }
}
";

#[test]
fn a_runaway_mutant_is_cut_off_long_before_its_budget() {
    if nested() {
        return;
    }
    let dir = workspace(HANGS);
    let started = std::time::Instant::now();
    let (code, output) = session(&dir, &["--ops", "relational.gt_to_ge", "--timeout", "120"]);
    let elapsed = started.elapsed();

    assert_eq!(code, EXIT_OK, "{output}");

    // The mutant hangs, so it must be reported as detected rather than as a survivor.
    assert!(output.contains("0 missed"), "{output}");

    // And the whole run must finish in far less than the single mutant's two-minute budget: the
    // point of the detector is that silence, not the budget, is what ends a hung run.
    assert!(elapsed < core::time::Duration::from_secs(60), "took {elapsed:?}: {output}");

    // The report says it stalled, and where. Which of the two forms appears depends on whether the
    // harness had finished announcing a test before it went quiet; here the only test is the one
    // that hangs, so there is nothing to name and saying so is the honest answer.
    assert!(output.contains("TIMEOUT"), "{output}");
    assert!(output.contains("stalled"), "{output}");
    assert!(output.contains("1 timed out,"), "{output}");
}

#[test]
fn stall_detection_can_be_turned_off() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--ops", "relational", "--no-stall-detection"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("Hangs"), "{output}");
}

#[test]
fn a_mutant_no_test_can_reach_is_reported_uncovered() {
    if nested() {
        return;
    }
    let dir = archipelago();
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");

    // The same mutant exists in both packages. The one the mainland's tests compile is caught; the
    // island's cannot be reached by any binary the build produced, which is a stronger statement
    // than "survived" and must not be reported as one.
    assert!(
        output.contains("0 missed,"),
        "the mainland's own tests must still kill its mutants: {output}"
    );

    // An uncovered mutant costs score without being a survivor, so it is counted on its own rather
    // than folded into the missed total a reader would go looking for an assertion for.
    assert!(
        output.contains("4 mutants (2 caught, 0 missed, 0 timed out, 0 out of memory, 2 uncovered => 50.0%)"),
        "{output}"
    );

    // Uncovered is a stronger statement than survived, so the island's mutants must not be listed
    // among the ones a test ran and failed to notice.
    assert!(!output.contains("MISSED"), "{output}");
}

#[test]
fn a_survivor_reaches_the_diff_and_the_security_tab() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);
    let sarif = Utf8PathBuf::from_path_buf(dir.path().join("out.sarif")).expect("path is not UTF-8");
    let summary_path = dir.path().join("summary.md");
    let host = FakeHost::piped()
        .with_env("GITHUB_ACTIONS", "true")
        .with_env("GITHUB_STEP_SUMMARY", summary_path.to_str().expect("path is not UTF-8"));

    let (code, out, err) = session_on(host, &dir, &["--ops", "stmt", "--sarif", sarif.as_str()]);

    assert_eq!(code, EXIT_OK, "{out}{err}");

    // Deleting the `push` survives, because the test only asserts that recording does not panic.
    assert!(out.contains("::warning file=src/lib.rs,"), "{out}");
    assert!(out.contains("entries"), "{out}");

    let log: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sarif).expect("the sarif log")).expect("valid json");

    assert_eq!(log["version"], "2.1.0");

    let results = log["runs"][0]["results"].as_array().expect("results");

    assert!(!results.is_empty(), "a survivor must reach the log");
    assert_eq!(results[0]["level"], "note");
    assert_eq!(
        results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "src/lib.rs"
    );

    let summary = fs::read_to_string(&summary_path).expect("the job summary");

    assert!(summary.contains("## Mutation testing"), "{summary}");
    assert!(summary.contains("**Score"), "{summary}");
}

#[test]
fn nothing_is_annotated_outside_a_runner() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    // The default is `auto`, and a developer at a terminal must not have workflow commands printed
    // into the results stream they are piping somewhere.
    let (code, out, err) = session_on(FakeHost::piped(), &dir, &["--ops", "stmt"]);

    assert_eq!(code, EXIT_OK, "{out}{err}");
    assert!(!out.contains("::warning"), "{out}");
}

#[test]
fn the_hidden_diag_flag_dumps_what_the_run_measured_about_itself() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    let (code, output) = session(&dir, &["--dry-run", "--diag"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("── diag ─"), "{output}");
    assert!(output.contains("by mutator"), "{output}");

    // A dry run built nothing, so there is nothing to say about a build or a baseline.
    assert!(!output.contains("baseline "), "{output}");
}

#[test]
fn the_diag_flag_stays_out_of_the_help_it_is_not_for_users_of_the_tool() {
    if nested() {
        return;
    }
    let mut host = FakeHost::piped();
    let code = run(&mut host, ["cargo-gamma", "gamma", "run", "--help"].map(str::to_owned).to_vec());
    let text = format!("{}{}", host.stdout(), host.stderr());

    assert_eq!(code, EXIT_OK, "{text}");
    assert!(!text.contains("--diag"), "{text}");
}

#[test]
fn a_real_run_that_finds_no_mutants_says_so_rather_than_reporting_a_perfect_score() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    // `--exclude` leaves nothing to mutate, so the run still copies, builds and baselines but has
    // no population to judge. Reporting 100% over an empty set would be the most misleading answer
    // the tool could give, so it says plainly that it generated nothing.
    let (code, output) = session(&dir, &["--exclude-file", "**/*.rs"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("no mutants were generated"), "{output}");
}

#[test]
fn leaking_the_scratch_tree_reports_where_it_was_kept() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    // The scratch tree is normally deleted, which makes a mutant that only reproduces inside the
    // instrumented copy impossible to inspect. `--leak-dirs` keeps it, and the path has to be
    // printed or the option leaves the user with a tree they cannot find.
    let (code, output) = session(&dir, &["--ops", "relational", "--leak-dirs"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("Kept"), "{output}");

    let tree = dir.path().join("target").join("gamma").join("tree");

    assert!(tree.exists(), "the scratch tree should have been kept at {}", tree.display());
}

#[test]
fn skipping_the_baseline_says_so_and_still_judges_the_mutants() {
    if nested() {
        return;
    }
    let dir = workspace(SUBJECT);

    // Measuring the baseline is the largest fixed cost of a run, and a user who already knows the
    // suite is green can skip it. The run then has no measured time to scale a timeout from, so it
    // has to say the measurement was skipped rather than report a baseline of zero.
    let (code, output) = session(&dir, &["--ops", "relational", "--no-baseline"]);

    // Without a measured baseline there is no elapsed time to scale a timeout from, so the run
    // falls back to the configured floor and still has to reach a verdict on every mutant.
    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("2 caught"), "{output}");
}

#[test]
fn a_default_run_completes_whether_or_not_the_host_can_bound_memory() {
    if nested() {
        return;
    }
    // Memory control is on by default, and most hosts a test runs on cannot provide it: a CI
    // container without cgroup delegation, or macOS at all. A default nobody asked for must not be
    // able to stop a run, so this asserts the run finishes and produces a score either way — and
    // that whichever path was taken, the fact is stated rather than left to be discovered later.
    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--ops", "relational"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(output.contains("Summary"), "{output}");

    let bounded = !output.contains("not bounded on this host");

    // The summary always carries the count, so a reader can tell an out-of-memory kill from an
    // ordinary one without having to know whether enforcement was available.
    assert!(output.contains("out of memory"), "{output}");

    if !bounded {
        assert!(output.contains("Memory"), "the note has to name itself so it can be searched for");
    }
}

#[test]
fn asking_for_memory_control_that_cannot_be_delivered_is_an_error() {
    if nested() {
        return;
    }
    // The inverse of the test above, and the reason `Demand` exists. Someone who passed `--memory`
    // wants a guarantee, so silently running without one is the single outcome that could cost them
    // the machine they were protecting.
    if cargo_gamma_lib::exec::memory::support().is_ok() {
        return;
    }

    let dir = workspace(SUBJECT);
    let (code, output) = session(&dir, &["--ops", "relational", "--memory", "enforce"]);

    assert_ne!(code, EXIT_OK, "{output}");
    assert!(output.contains("not available here"), "{output}");
}

#[test]
fn a_mutant_in_code_the_feature_set_excludes_is_not_reported_as_a_survivor() {
    if nested() {
        return;
    }
    // The bug this pins down was silent and expensive: mutants behind an inactive `#[cfg]` were
    // generated, compiled away to nothing, killed by no test and reported as survivors, so a run
    // could name a page of unfixable failures and quote a score tens of points below the truth.
    let dir = conditional();
    let (code, output) = session(&dir, &["--ops", "expr"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(
        !output.contains("MISSED src/extra.rs"),
        "an unbuilt mutant was blamed on the tests: {output}"
    );
    assert!(output.contains("not built"), "the summary has to account for them: {output}");

    // A count alone leaves the reader with a smaller population than `gamma list` promised and no
    // way to find out why, so the note naming the remedy is part of the fix rather than decoration.
    assert!(output.contains("Features"), "{output}");
}

#[test]
fn turning_the_feature_on_brings_the_same_code_back_into_the_run() {
    if nested() {
        return;
    }
    // The other half of the pair. Excusing a mutant is only correct while the compiler really is
    // ignoring its file; a rule that quietly excused it either way would hide real gaps in the
    // suite, which is a worse failure than the one it replaced.
    let dir = conditional();
    let (code, output) = session(&dir, &["--ops", "expr", "--features", "extra"]);

    assert_eq!(code, EXIT_OK, "{output}");
    assert!(!output.contains("not built"), "{output}");
}
