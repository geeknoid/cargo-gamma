//! End-to-end tests of the command-line surface, driven through a fake host.

mod common;

use camino::Utf8PathBuf;
use cargo_gamma_lib::run;
use common::FakeHost;
use std::fs;
use tempfile::TempDir;

/// Exit code for a run in which every gate passed.
const EXIT_OK: i32 = 0;

/// Exit code for a usage error.
const EXIT_USAGE: i32 = 1;

/// Builds a throwaway single-package workspace containing `source` as its library.
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

/// Runs the tool against a directory and returns the exit code and captured host.
fn invoke(dir: &TempDir, args: &[&str]) -> (i32, FakeHost) {
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut command = vec!["cargo-gamma".to_owned(), "gamma".to_owned()];

    command.extend(args.iter().map(|arg| (*arg).to_owned()));

    // `explain` reads only the registry, so it takes no directory.
    if args.first() != Some(&"explain") {
        command.push("--dir".to_owned());
        command.push(path.to_string());
    }

    let mut host = FakeHost::piped();
    let code = run(&mut host, command);

    (code, host)
}

const SUBJECT: &str = "
/// Returns whether the value is in range.
pub fn in_range(value: i32, limit: i32) -> bool {
    value < limit
}

/// Adds a margin.
pub fn with_margin(value: i32) -> i32 {
    value + 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_work() {
        assert!(in_range(1, 2));
    }
}
";

#[test]
fn listing_mutants_reports_the_expected_operators() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.stdout();

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(output.contains("relational.lt_to_le"), "{output}");
    assert!(output.contains("arith.add_to_sub"), "{output}");
}

#[test]
fn listing_mutants_reports_the_file_line_and_column() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.stdout();

    assert!(output.contains("src/lib.rs:4:5"), "{output}");
}

#[test]
fn test_modules_are_not_mutated() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.stdout();

    assert!(!output.contains("ranges_work"), "{output}");
    assert!(!output.contains("literal.int_to_zero]") || !output.contains("assert"), "{output}");
}

#[test]
fn doc_comments_are_not_reported_as_string_literals() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);
    let output = host.stdout();

    assert!(!output.contains("Returns whether the value is in range"), "{output}");
}

#[test]
fn selecting_one_operator_excludes_the_others() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--ops", "relational"]);
    let output = host.stdout();

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(output.contains("relational."), "{output}");
    assert!(!output.contains("arith."), "{output}");
}

#[test]
fn a_negated_selector_carves_out_of_a_family() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants", "--ops", "relational,!relational.lt_to_le"]);
    let output = host.stdout();

    assert!(output.contains("relational.lt_to_gt"), "{output}");
    assert!(!output.contains("relational.lt_to_le"), "{output}");
}

#[test]
fn an_unknown_selector_is_a_usage_error() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--ops", "relationl"]);

    assert_eq!(code, EXIT_USAGE, "{}", host.stdout());
    assert!(host.stderr().contains("did you mean `relational`"), "{}", host.stderr());
}

#[test]
fn an_out_of_range_shard_is_a_usage_error() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--shard-count", "2", "--shard-index", "5"]);

    assert_eq!(code, EXIT_USAGE, "{}", host.stdout());
    assert!(host.stderr().contains("out of range"), "{}", host.stderr());
}

#[test]
fn shards_partition_the_mutants_exactly() {
    let dir = workspace(SUBJECT);
    let (_, whole) = invoke(&dir, &["list", "mutants"]);
    let total = whole.stdout().lines().count();

    let parts: usize = (0..3)
        .map(|index| {
            let (code, host) = invoke(
                &dir,
                &["list", "mutants", "--shard-count", "3", "--shard-index", &index.to_string()],
            );

            assert_eq!(code, EXIT_OK, "{}", host.stderr());
            host.stdout().lines().count()
        })
        .sum();

    assert_eq!(parts, total, "sharding lost or duplicated mutants");
}

#[test]
fn shard_membership_is_deterministic() {
    let dir = workspace(SUBJECT);
    let (_, first) = invoke(&dir, &["list", "mutants", "--shard-count", "3", "--shard-index", "1"]);
    let (_, second) = invoke(&dir, &["list", "mutants", "--shard-count", "3", "--shard-index", "1"]);

    assert_eq!(first.stdout(), second.stdout());
}

#[test]
fn file_filters_narrow_the_scan() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--file", "**/lib.rs"]);

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(!host.stdout().trim().is_empty(), "{}", host.stdout());
}

#[test]
fn a_file_pattern_matching_nothing_is_a_usage_error() {
    // Silently reporting no mutants and exiting zero reads in CI exactly like a clean run, so a
    // typo in a checked-in pattern could hide a whole crate from mutation testing indefinitely.
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--file", "nothing_matches.rs"]);

    assert_eq!(code, EXIT_USAGE, "{}", host.stderr());
    assert!(host.stderr().contains("no source file matches"), "{}", host.stderr());
}

#[test]
fn excluded_files_are_not_scanned() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants", "--exclude-file", "lib.rs"]);

    assert!(host.stdout().trim().is_empty(), "{}", host.stdout());
}

#[test]
fn json_output_is_valid_json() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "mutants", "--json"]);

    assert_eq!(code, EXIT_OK, "{}", host.stderr());

    let parsed: serde_json::Value = serde_json::from_str(&host.stdout()).expect("output was not valid JSON");

    assert!(parsed.as_array().is_some_and(|items| !items.is_empty()));
}

#[test]
fn json_mutants_carry_a_stable_id() {
    let dir = workspace(SUBJECT);
    let (_, first) = invoke(&dir, &["list", "mutants", "--json"]);
    let (_, second) = invoke(&dir, &["list", "mutants", "--json"]);

    let left: serde_json::Value = serde_json::from_str(&first.stdout()).expect("not JSON");
    let right: serde_json::Value = serde_json::from_str(&second.stdout()).expect("not JSON");

    let ids = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .expect("not an array")
            .iter()
            .map(|item| item["id"].as_str().expect("no id").to_owned())
            .collect()
    };

    let identifiers = ids(&left);

    assert!(!identifiers.is_empty());
    assert_eq!(identifiers, ids(&right));
}

#[test]
fn listing_files_reports_the_library() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "files"]);

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stdout().contains("src/lib.rs"), "{}", host.stdout());
}

#[test]
fn listing_ops_marks_the_enabled_set() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["list", "ops"]);
    let output = host.stdout();

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(output.contains("relational.lt_to_le"), "{output}");
    assert!(output.contains("* = enabled by the current selection"), "{output}");
}

#[test]
fn a_run_reports_what_it_found() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["run", "--dry-run"]);

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stdout().contains("mutants in"), "{}", host.stdout());
}

#[test]
fn a_run_with_no_subcommand_behaves_like_run() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["--dry-run"]);

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stdout().contains("mutants in"), "{}", host.stdout());
}

#[test]
fn results_go_to_stdout_and_progress_goes_to_stderr() {
    // A user piping `list` into another program must not receive progress chatter in the pipe.
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["list", "mutants"]);

    assert!(!host.stdout().is_empty());
    assert!(!host.stdout().contains("Scanning"), "{}", host.stdout());
}

#[test]
fn a_terminal_host_is_colorized() {
    let dir = workspace(SUBJECT);
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::terminal(100);

    let code = run(
        &mut host,
        [
            "cargo-gamma",
            "gamma",
            "run",
            "--dry-run",
            "--color",
            "always",
            "--dir",
            path.as_str(),
        ],
    );

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stdout().contains('\x1b'), "{:?}", host.stdout());
}

#[test]
fn a_piped_host_is_not_colorized() {
    let dir = workspace(SUBJECT);
    let (_, host) = invoke(&dir, &["run", "--dry-run"]);

    assert!(!host.stdout().contains('\x1b'), "{:?}", host.stdout());
}

#[test]
fn explaining_a_mutator_describes_how_to_suppress_it() {
    let dir = workspace(SUBJECT);
    let (code, host) = invoke(&dir, &["explain", "relational.lt_to_le"]);
    let output = host.stdout();

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(output.contains("replace < with <="), "{output}");
    assert!(output.contains("// #[gamma::skip(relational.lt_to_le)]"), "{output}");
}

#[test]
fn explaining_an_unknown_subject_is_a_usage_error() {
    let dir = workspace(SUBJECT);
    let (code, _) = invoke(&dir, &["explain", "not_a_mutator"]);

    assert_eq!(code, EXIT_USAGE);
}

#[test]
fn help_goes_to_stdout_and_exits_zero() {
    let mut host = FakeHost::piped();
    let code = run(&mut host, ["cargo-gamma", "gamma", "--help"]);

    assert_eq!(code, EXIT_OK);
    assert!(host.stdout().contains("mutation testing"), "{}", host.stdout());
    assert!(host.stderr().is_empty(), "{}", host.stderr());
}

#[test]
fn an_unknown_flag_goes_to_stderr_and_exits_one() {
    let mut host = FakeHost::piped();
    let code = run(&mut host, ["cargo-gamma", "gamma", "--not-a-flag"]);

    assert_eq!(code, EXIT_USAGE);
    assert!(!host.stderr().is_empty());
    assert!(host.stdout().is_empty(), "{}", host.stdout());
}

#[test]
fn the_tool_works_when_invoked_directly_rather_than_through_cargo() {
    let mut host = FakeHost::piped();
    let code = run(&mut host, ["cargo-gamma", "--help"]);

    assert_eq!(code, EXIT_OK);
    assert!(host.stdout().contains("mutation testing"), "{}", host.stdout());
}

#[test]
fn a_directory_that_is_not_a_workspace_fails_without_panicking() {
    let dir = TempDir::new().expect("could not create a temporary directory");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("path is not UTF-8");
    let mut host = FakeHost::piped();

    let code = run(&mut host, ["cargo-gamma", "gamma", "list", "mutants", "--dir", path.as_str()]);

    assert_ne!(code, EXIT_OK);
    assert!(host.stderr().contains("cargo metadata"), "{}", host.stderr());
}

#[test]
fn a_file_that_does_not_parse_names_the_file() {
    let dir = workspace("pub fn broken( {");
    let (code, host) = invoke(&dir, &["list", "mutants"]);

    assert_ne!(code, EXIT_OK);
    assert!(host.stderr().contains("could not parse"), "{}", host.stderr());
    assert!(host.stderr().contains("lib.rs"), "{}", host.stderr());
}

#[test]
fn an_empty_library_yields_no_mutants() {
    let dir = workspace("");
    let (code, host) = invoke(&dir, &["list", "mutants"]);

    assert_eq!(code, EXIT_OK, "{}", host.stderr());
    assert!(host.stdout().trim().is_empty());
}
