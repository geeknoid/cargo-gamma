//! Translating a foreign command line into the equivalent `cargo gamma` one.

use super::config::cargo_arg;

/// Translates a foreign command line into the equivalent `cargo gamma` one.
///
/// This exists because the CLI was redesigned rather than copied, which turns every CI workflow in
/// the wild into a small migration cost. Handing back the translated line makes that cost a command
/// rather than an afternoon of reading two help texts side by side.
///
/// Unrecognized flags are returned as notes rather than guessed at: a wrong translation that looks
/// right is worse than an honest gap.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "the complete foreign CLI vocabulary is translated in one ordered loop"
)]
pub fn translate_command(args: &[String]) -> (Vec<String>, Vec<String>) {
    // Decided before the loop rather than when `--list` turns up in it. The subcommand governs which
    // flags exist: `list` carries the selection and nothing else, so `-j`, the timeouts and the
    // cargo passthroughs are not merely useless there, they are rejected by the parser. Rewriting
    // the subcommand after those flags had already been emitted handed the user a line that does
    // not run — which is the failure the doc above rules out, wearing the other face.
    let listing = args.iter().any(|arg| arg == "--list");
    let mut out = vec![
        "cargo".to_owned(),
        "gamma".to_owned(),
        if listing { "list" } else { "run" }.to_owned(),
    ];

    if listing {
        out.push("mutants".to_owned());
    }

    let mut notes = Vec::new();
    let mut rest = args.iter().skip_while(|arg| *arg == "cargo" || *arg == "mutants");

    // A macro rather than a closure: both this and the arms below need `rest`, and a closure would
    // hold the only mutable borrow of it for the whole loop.
    macro_rules! take {
        ($flag:expr) => {{
            if let Some(value) = rest.next() {
                out.push($flag.to_owned());
                out.push(value.clone());
            } else {
                notes.push(format!("{} needs a value", $flag));
            }
        }};
    }

    // The same, for the flags that only exist on `run`. The source tool accepts them alongside
    // `--list` and ignores them; here they become notes, because a dropped flag the user can read
    // is worth more than a command line that will not parse.
    macro_rules! measuring {
        ($flag:expr) => {{
            if listing {
                match rest.next() {
                    Some(value) => notes.push(format!("{} {value} dropped: `list` does not build or run anything", $flag)),
                    None => notes.push(format!("{} needs a value", $flag)),
                }
            } else {
                take!($flag);
            }
        }};

        ($flag:expr, bare) => {{
            if listing {
                notes.push(format!("{} dropped: `list` does not build or run anything", $flag));
            } else {
                out.push($flag.to_owned());
            }
        }};
    }

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-f" | "--file" => take!("--file"),
            "-e" | "--exclude" => take!("--exclude-file"),
            "-j" | "--jobs" => measuring!("-j"),
            "-d" | "--dir" => take!("--dir"),
            "-t" | "--timeout" => foreign_timeout(&mut notes, arg, listing, rest.next()),
            "--timeout-multiplier" => measuring!("--test-timeout-multiplier"),
            "--minimum-test-timeout" => measuring!("--minimum-test-timeout"),
            "--build-timeout" => measuring!("--build-timeout"),
            "--build-timeout-multiplier" => measuring!("--build-timeout-multiplier"),
            "-D" | "--in-diff" => take!("--in-diff"),
            "-p" | "--package" => take!("--package"),
            "--test-package" => measuring!("--test-package"),
            "--test-tool" => match rest.next().map(String::as_str) {
                Some("nextest") => measuring!("--nextest", bare),
                Some("cargo") => {}
                Some(other) => notes.push(format!("--test-tool {other} has no gamma equivalent")),
                None => notes.push("--test-tool needs a tool name".to_owned()),
            },
            "--error" => take!("--error"),
            "--features" => take!("--features"),

            "-F" | "--re" | "-E" | "--exclude-re" => regex_filter(&mut notes, arg, rest.next()),

            "--profile" => measuring!("--profile"),
            "-C" | "--cargo-arg" => {
                if listing {
                    measuring!("--cargo-arg");
                } else {
                    cargo_arg(&mut out, &mut notes, rest.next());
                }
            }
            "--cargo-test-arg" => measuring!("--cargo-test-arg"),

            // `--iterate` in cargo-mutants is gamma's default behavior (--incremental full).
            "--iterate" => {
                notes.push("--iterate is gamma's default behavior (--incremental full); no flag needed".to_owned());
            }

            "--all-features" => out.push("--all-features".to_owned()),
            "--no-default-features" => out.push("--no-default-features".to_owned()),
            // The source spells this as a flag taking a boolean; gamma's is a plain flag.
            "--test-workspace" => match rest.next().map(String::as_str) {
                Some("true") => measuring!("--test-workspace", bare),
                Some("false") => {}
                _ => notes.push("--test-workspace needs true or false".to_owned()),
            },
            "--workspace" => out.push("--workspace".to_owned()),
            "--leak-dirs" => measuring!("--leak-dirs", bare),
            "--no-config" => out.push("--no-config".to_owned()),
            "--config" => take!("--config"),

            "--shard" => shard(&mut out, &mut notes, rest.next()),

            // Already answered, before the first flag was translated: this is where the subcommand
            // came from.
            "--list" => {}

            "--json" => out.push("--json".to_owned()),

            // Gamma orders mutants deterministically by design, which is what makes sharding stable
            // across runs, so there is nothing to turn off.
            "--no-shuffle" | "--shuffle" => {
                notes.push(format!("{arg} dropped: gamma's mutant order is always deterministic"));
            }

            "--baseline" => match rest.next().map(String::as_str) {
                Some("skip") => measuring!("--no-baseline", bare),
                Some(other) => notes.push(format!("--baseline {other} is the default")),
                None => notes.push("--baseline needs a value".to_owned()),
            },

            other if other.starts_with('-') => notes.push(format!("{other} has no gamma equivalent")),

            other => notes.push(format!("ignored positional argument `{other}`")),
        }
    }

    (out, notes)
}

/// Drops the whole-suite timeout with an explanation.
fn foreign_timeout(notes: &mut Vec<String>, arg: &str, listing: bool, value: Option<&String>) {
    if listing {
        notes.push(format!("{arg} dropped: `list` does not build or run anything"));
    } else if let Some(value) = value {
        notes.push(format!(
            "{arg} {value} dropped: gamma applies `--test-timeout-multiplier` per test binary rather than a whole-suite timeout"
        ));
    } else {
        notes.push(format!("{arg} needs a duration"));
    }
}

/// Translates `k/n` into the two flags that spell it here.
///
/// Two flags rather than one, because one option that silently means two numbers is the kind of
/// thing people get backwards.
fn shard(out: &mut Vec<String>, notes: &mut Vec<String>, spec: Option<&String>) {
    let Some((index, count)) = spec.and_then(|spec| spec.split_once('/')) else {
        notes.push("--shard needs an INDEX/COUNT value".to_owned());
        return;
    };

    out.push("--shard-index".to_owned());
    out.push(index.to_owned());
    out.push("--shard-count".to_owned());
    out.push(count.to_owned());
}

/// Reports a regex filter as dropped, and says what to reach for instead.
///
/// `-F` is the short form of `--re` there, not of `--features`: the source tool matches that regex
/// against the names it prints for its own mutants. Gamma prints different names, so there is
/// nothing honest to translate it into — and reading `-F` as `--features` turned a filter into a
/// feature list, which compiles something else entirely.
fn regex_filter(notes: &mut Vec<String>, flag: &str, pattern: Option<&String>) {
    match pattern {
        Some(pattern) => notes.push(format!(
            "{flag} {pattern} dropped: gamma has no regex filter; narrow `--mutators` or suppress the site"
        )),
        None => notes.push(format!("{flag} needs a regex")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::translate;
    use super::*;

    /// The option lines of `cargo mutants --help`, verbatim, from cargo-mutants 27.0.0.
    ///
    /// Checked in so that arity is a fact the tests can hold rather than something remembered: a
    /// flag written with a `<PLACEHOLDER>` takes the next argument, and one written bare does not.
    const CARGO_MUTANTS_HELP: &str = "\
  -h, --help
      --cap-lints <CAP_LINTS>
      --profile <PROFILE>
      --config <FILE>
      --no-config
      --copy-target <COPY_TARGET>
      --copy-vcs <COPY_VCS>
      --gitignore <GITIGNORE>
      --in-place
      --leak-dirs
  -L, --level <LEVEL>
      --Zmutate-file <FILE>
      --baseline <BASELINE>
      --build-timeout-multiplier <BUILD_TIMEOUT_MULTIPLIER>
      --build-timeout <BUILD_TIMEOUT>
  -C, --cargo-arg <CARGO_ARG>
      --cargo-test-arg <CARGO_TEST_ARG>
      --check
  -j, --jobs <JOBS>
      --jobserver <JOBSERVER>
      --jobserver-tasks <JOBSERVER_TASKS>
      --list
      --list-files
      --minimum-test-timeout <MINIMUM_TEST_TIMEOUT>
      --no-shuffle
      --shard <SHARD>
      --shuffle
  -t, --timeout <TIMEOUT>
      --timeout-multiplier <TIMEOUT_MULTIPLIER>
      --test-tool <TEST_TOOL>
      --sharding <SHARDING>
      --features <FEATURES>
      --no-default-features
      --all-features
  -F, --re <EXAMINE_RE>
  -e, --exclude <EXCLUDE>
  -E, --exclude-re <EXCLUDE_RE>
  -f, --file <FILE>
  -D, --in-diff <IN_DIFF>
      --iterate
  -p, --package <package>
      --skip-calls <SKIP_CALLS>
      --skip-calls-defaults <SKIP_CALLS_DEFAULTS>
      --workspace
      --error <ERROR>
  -d, --dir <DIR>
      --manifest-path <MANIFEST_PATH>
      --completions <COMPLETIONS>
      --version
      --all-logs
      --annotations <ANNOTATIONS>
  -v, --killed
      --colors <COLORS>
      --json
      --line-col <LINE_COL>
      --no-times
  -o, --output <OUTPUT>
  -V, --unviable
      --diff
      --test-package <TEST_PACKAGE>
      --test-workspace <TEST_WORKSPACE>
      --emit-schema <EMIT_SCHEMA>
";

    /// Every flag in the help text, paired with whether it consumes the following argument.
    fn foreign_flags() -> Vec<(&'static str, bool)> {
        CARGO_MUTANTS_HELP
            .lines()
            .flat_map(|line| {
                let takes_value = line.contains('<');

                line.split(',')
                    .map(str::trim)
                    .filter(|token| token.starts_with('-'))
                    .filter_map(move |token| token.split_whitespace().next().map(|flag| (flag, takes_value)))
            })
            .collect()
    }

    #[test]
    fn a_command_line_is_translated_flag_by_flag() {
        let args = ["cargo", "mutants", "--file", "src/**", "-j", "4"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run --file src/** -j 4");
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// `take!` owns every direct value-to-value translation. A source flag with no value must not
    /// leave its destination spelling behind, because that turns one malformed command into
    /// another and can make the following option look like its value.
    #[test]
    fn a_missing_value_drops_the_translated_flag_and_reports_the_omission() {
        let args = ["cargo", "mutants", "--file"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out, ["cargo", "gamma", "run"].map(str::to_owned));
        assert!(notes.iter().any(|note| note == "--file needs a value"), "{notes:?}");
    }

    #[test]
    fn a_shard_becomes_two_explicit_flags() {
        // `3/8` reads as a fraction, and people write the two numbers the wrong way round often
        // enough that spelling them out is worth the extra characters.
        let args = ["cargo", "mutants", "--shard", "3/8"].map(str::to_owned);
        let (out, _) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run --shard-index 3 --shard-count 8");
    }

    #[test]
    fn listing_becomes_the_list_subcommand() {
        let args = ["cargo", "mutants", "--list", "--json"].map(str::to_owned);
        let (out, _) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma list mutants --json");
    }

    /// The source tool accepts `--list -j 4` and ignores the jobs; `cargo gamma list` rejects it,
    /// because listing carries the selection and nothing else. A translated line that does not
    /// parse is the same failure as a wrong translation, so the flag is dropped and said out loud.
    #[test]
    fn a_flag_that_only_exists_on_run_becomes_a_note_when_the_line_is_a_listing() {
        let args = ["cargo", "mutants", "--list", "-j", "4"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma list mutants");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("-j 4"), "{notes:?}");
    }

    /// The order the flags arrive in must not decide it either: `-j` before `--list` was the
    /// case that emitted the flag and then rewrote the subcommand underneath it.
    #[test]
    fn a_run_only_flag_ahead_of_the_listing_flag_is_dropped_just_the_same() {
        let args = ["cargo", "mutants", "-j", "4", "--timeout", "20", "--list"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma list mutants");
        assert_eq!(notes.len(), 2, "{notes:?}");
    }

    /// Selection is what `list` is for, so none of it is dropped.
    #[test]
    fn the_selection_survives_a_listing_intact() {
        let args = ["cargo", "mutants", "--list", "-d", "crates/x", "--file", "src/lib.rs"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma list mutants --dir crates/x --file src/lib.rs");
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn skipping_the_baseline_carries_over() {
        let args = ["cargo", "mutants", "--baseline", "skip"].map(str::to_owned);
        let (out, _) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run --no-baseline");
    }

    #[test]
    fn an_unrecognized_flag_becomes_a_note_rather_than_a_guess() {
        // A translation that looks right and is wrong will be copied into CI and believed.
        let args = ["cargo", "mutants", "--cap-lints", "true"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.iter().any(|note| note.contains("--cap-lints")), "{notes:?}");
    }

    #[test]
    fn shuffling_flags_are_dropped_with_an_explanation() {
        let args = ["cargo", "mutants", "--no-shuffle"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes[0].contains("deterministic"), "{notes:?}");
    }

    #[test]
    fn a_report_destination_becomes_a_reporters_table() {
        // The source writes its whole output tree under one directory; gamma points each report
        // at a path of its own. `output` therefore has no equivalent and is commented out, while
        // `html_report` does have one and moves into `[reporters]`.
        let out = translate("output = \"target/mutants\"\nhtml_report = \"target/report.html\"\n").expect("translates");

        assert!(out.text.contains("[reporters]"), "{}", out.text);
        assert!(out.text.contains("html = \"target/report.html\""), "{}", out.text);
        assert!(out.text.contains("# output ="), "the untranslatable key must survive as a comment");

        let config = crate::config::Config::parse(&out.text).expect("the generated file must load");

        assert_eq!(config.reporters.html.as_deref(), Some(camino::Utf8Path::new("target/report.html")));
    }

    #[test]
    fn a_test_workspace_flag_set_to_false_translates_to_nothing() {
        // The source spells this as a flag taking a boolean, so `false` is a real spelling of
        // the default. Emitting `--test-workspace` for it would widen every run that used it.
        let args = ["cargo", "mutants", "--test-workspace", "false"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_test_workspace_flag_with_no_boolean_becomes_a_note() {
        let args = ["cargo", "mutants", "--test-workspace"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.iter().any(|note| note.contains("--test-workspace")), "{notes:?}");
    }

    #[test]
    fn a_shard_without_a_value_becomes_a_note_rather_than_a_broken_command() {
        // Half a shard specification would produce a command line that fails at parse time in CI,
        // which is a worse place to find out than the migration itself.
        let args = ["cargo", "mutants", "--shard"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.iter().any(|note| note.contains("INDEX/COUNT")), "{notes:?}");
    }

    #[test]
    fn a_baseline_value_that_is_already_the_default_becomes_a_note() {
        let args = ["cargo", "mutants", "--baseline", "run"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.iter().any(|note| note.contains("default")), "{notes:?}");
    }

    #[test]
    fn a_baseline_flag_with_no_value_becomes_a_note() {
        let args = ["cargo", "mutants", "--baseline"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.iter().any(|note| note.contains("--baseline")), "{notes:?}");
    }

    /// The bug this guards: `--iterate` is a bare switch there, and it was routed through the arm
    /// that consumes a value — so `--iterate -e src/foo.rs` silently lost the exclusion, widening
    /// the population and biasing the score down with nothing to see.
    #[test]
    fn iterate_is_a_bare_switch_and_does_not_swallow_what_follows() {
        let args = ["cargo", "mutants", "--iterate", "-e", "src/foo.rs"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run --exclude-file src/foo.rs");
        assert!(notes.iter().any(|note| note.contains("default behavior")), "{notes:?}");
    }

    /// `-F` is the short form of `--re` there, not of `--features`: reading it as a feature list
    /// turned a mutant filter into a different build.
    #[test]
    fn a_regex_filter_is_not_mistaken_for_a_feature_list() {
        let args = ["cargo", "mutants", "-F", "^replace"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.iter().any(|note| note.contains("no regex filter")), "{notes:?}");
    }

    #[test]
    fn test_tool_nextest_translates_to_nextest_flag() {
        let args = ["cargo", "mutants", "--test-tool", "nextest"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run --nextest");
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn test_tool_cargo_translates_to_default_runner() {
        let args = ["cargo", "mutants", "--test-tool", "cargo"].map(str::to_owned);
        let (out, notes) = translate_command(&args);

        assert_eq!(out.join(" "), "cargo gamma run");
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// The bug this guards, in the general form: `--iterate` was translated as taking a value when
    /// it takes none, so the argument after it disappeared. Nothing about that is visible in the
    /// output — the run simply tests a different population — so the arity of every arm is checked
    /// against the source tool's own help rather than against memory.
    #[test]
    fn every_translated_flag_has_the_arity_the_source_tool_gives_it() {
        const SENTINEL: &str = "SENTINEL";

        let flags = foreign_flags();

        assert!(flags.len() > 40, "the help text must parse into flags: {flags:?}");

        for (flag, takes_value) in flags {
            let args = ["cargo".to_owned(), "mutants".to_owned(), flag.to_owned(), SENTINEL.to_owned()];
            let (_out, notes) = translate_command(&args);

            // A flag this tool does not claim to translate says so, and what follows it is not its
            // business either way.
            if notes.iter().any(|note| note.contains("has no gamma equivalent")) {
                continue;
            }

            let swallowed = !notes
                .iter()
                .any(|note| note.contains(&format!("ignored positional argument `{SENTINEL}`")));

            assert_eq!(
                swallowed,
                takes_value,
                "`{flag}` {} a value in cargo mutants, and the translation {} one: {notes:?}",
                if takes_value { "takes" } else { "does not take" },
                if swallowed { "consumed" } else { "did not consume" }
            );
        }
    }
}
