use std::ffi::OsString;
use std::io::Write;

use clap::Parser;
use clap::error::ErrorKind;

use crate::config::Config;
use crate::report::Styler;

use super::cli::{Cli, Command};
use super::completions::completions;
use super::explain::explain;
use super::suppress::suppress;
use super::host::Host;
use super::list::list;
use super::merge::merge;
use super::migrate::migrate;
use super::run::{configure, run_session};

/// The multiple of the baseline duration a mutant is allowed when nothing says otherwise.
///
/// Tight on purpose: a mutant that hangs costs its whole budget, and that budget is paid once per
/// hang across the population, so a generous multiplier is one of the few ways a run can still take
/// far longer than the cost model predicts. The floor keeps a fast suite from reading scheduler
/// noise as a hang, and stall detection catches the common hangs long before the budget expires.
pub(super) const DEFAULT_TIMEOUT_MULTIPLIER: f64 = 1.2;

/// Exit code for a run in which every gate passed.
pub const EXIT_OK: i32 = 0;

/// Exit code for a usage error: bad arguments or bad configuration.
pub const EXIT_USAGE: i32 = 1;

/// Exit code for a run that completed but in which some gate failed.
pub const EXIT_GATE_FAILED: i32 = 2;

/// Exit code for a run that could not proceed.
pub const EXIT_CANNOT_PROCEED: i32 = 3;

/// Exit code for an internal error.
pub const EXIT_INTERNAL: i32 = 70;

/// Runs the tool and returns the process exit code.
///
/// This returns rather than exits so that every path through the CLI, including the failure paths
/// and the exit codes themselves, is reachable from an ordinary integration test.
pub fn run<H: Host>(host: &mut H, args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> i32 {
    let normalized = normalize(args);

    let cli = match Cli::try_parse_from(normalized) {
        Ok(cli) => cli,

        Err(cause) => {
            // clap renders help and version to stdout and errors to stderr, matching cargo.
            let is_help = matches!(
                cause.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );

            let text = cause.render().ansi().to_string();

            if is_help {
                let _ = write!(host.output(), "{text}");
                return EXIT_OK;
            }

            let _ = write!(host.error(), "{text}");
            return EXIT_USAGE;
        }
    };

    let styler = Styler::new(cli.color.resolve(host.is_terminal()));

    match dispatch(host, cli, styler) {
        Ok(code) => code,

        Err(cause) => {
            let code = if cause.is_usage() { EXIT_USAGE } else { EXIT_CANNOT_PROCEED };
            let label = styler.error("error:");
            let mut stream = host.error();

            let _ = writeln!(stream, "{label} {cause}");

            code
        }
    }
}

/// Strips the argument cargo inserts when invoking a subcommand.
///
/// Invoked as `cargo gamma ...`, the process sees `["cargo-gamma", "gamma", ...]`. Invoked
/// directly as `cargo-gamma ...` it does not. Both must work, so drop a second argument that is
/// exactly `gamma` and nothing else.
fn normalize(args: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> Vec<OsString> {
    let mut normalized: Vec<OsString> = args.into_iter().map(Into::into).collect();

    if normalized.get(1).is_some_and(|entry| entry == "gamma") {
        let _ = normalized.remove(1);
    }

    if !normalized.is_empty() && implies_run(normalized.get(1..).unwrap_or_default()) {
        normalized.insert(1, "run".into());
    }

    normalized
}

/// The top-level options that may legitimately appear before a subcommand.
///
/// Each takes one value, which has to be stepped over when looking for the subcommand.
const GLOBAL_OPTIONS: [&str; 2] = ["--color", "--progress"];

/// Whether `args` is a bare `run` invocation with the word `run` left off.
///
/// The rule is deliberately shallow: after stepping over the global options, an argument that
/// begins with a dash cannot be a subcommand, so `run` is what was meant. Anything else is left
/// exactly as written, including a misspelled subcommand — clap's "did you mean" is far more useful
/// there than an unexpected-argument error from a `run` the user never asked for.
fn implies_run(args: &[OsString]) -> bool {
    let mut rest = args;

    while let Some(first) = rest.first().and_then(|entry| entry.to_str()) {
        if GLOBAL_OPTIONS
            .iter()
            .any(|option| first.strip_prefix(option).is_some_and(|rest| rest.starts_with('=')))
        {
            rest = &rest[1..];
        } else if GLOBAL_OPTIONS.contains(&first) {
            // The value is skipped along with the option, or a `--color never merge` would look
            // like it begins with the word `never`.
            rest = rest.get(2..).unwrap_or_default();
        } else {
            break;
        }
    }

    let Some(first) = rest.first().and_then(|entry| entry.to_str()) else {
        // Nothing at all means a default run, which is the shortest path into the tool.
        return true;
    };

    // Help and version are answered by the top-level parser; routing them through `run` would print
    // that subcommand's page instead of the overview the user asked for.
    first.starts_with('-') && !matches!(first, "-h" | "--help" | "-V" | "--version")
}

/// Runs the parsed command.
///
/// The configuration file is folded into the arguments here rather than inside each command, so
/// there is exactly one place where precedence between the file and the command line is decided.
pub(super) fn dispatch<H: Host>(host: &mut H, cli: Cli, styler: Styler) -> crate::Result<i32> {
    match cli.command {
        Command::Run(mut args) => {
            configure(host, &mut args, styler)?;
            run_session(host, &args, cli.progress, styler)
        }

        Command::List(mut args) => {
            Config::resolve(&args.select)?.apply_selection(&mut args.select);
            list(host, &args)
        }

        Command::Explain(args) => explain(host, &args),
        Command::Migrate(args) => migrate(host, &args, styler),

        Command::Suppress(mut args) => {
            configure(host, &mut args.run, styler)?;
            suppress(host, &args, cli.progress, styler)
        }

        Command::Merge(args) => merge(host, &args, styler),

        Command::Completions(args) => Ok(completions(host, &args)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the shell scripts is that they reach the results stream.
    #[test]
    fn completions_are_dispatched_to_the_results_stream() {
        let mut host = crate::testing::Sink::default();

        let code = run(&mut host, ["cargo-gamma", "gamma", "completions", "bash"]);

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("cargo-gamma"), "{}", host.out());
    }

    #[test]
    fn cargos_inserted_argument_is_stripped() {
        let normalized = normalize(["cargo-gamma", "gamma", "list"]);

        assert_eq!(normalized, vec!["cargo-gamma", "list"]);
    }

    #[test]
    fn direct_invocation_is_left_alone() {
        let normalized = normalize(["cargo-gamma", "list"]);

        assert_eq!(normalized, vec!["cargo-gamma", "list"]);
    }

    #[test]
    fn only_the_second_argument_named_gamma_is_stripped() {
        let normalized = normalize(["cargo-gamma", "list", "gamma"]);

        assert_eq!(normalized, vec!["cargo-gamma", "list", "gamma"]);
    }

    #[test]
    fn an_empty_argument_list_does_not_panic() {
        assert!(normalize(Vec::<String>::new()).is_empty());
    }

    #[test]
    fn a_bare_invocation_implies_run() {
        assert_eq!(normalize(["cargo-gamma", "gamma"]), vec!["cargo-gamma", "run"]);
    }

    #[test]
    fn a_leading_option_implies_run() {
        // The top level accepts no options of its own beyond the two globals, so an option here can
        // only have been meant for `run`.
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--ops", "relational"]),
            vec!["cargo-gamma", "run", "--ops", "relational"]
        );
    }

    #[test]
    fn a_named_subcommand_is_not_second_guessed() {
        for command in ["run", "list", "explain", "migrate", "suppress", "merge", "help"] {
            assert_eq!(normalize(["cargo-gamma", "gamma", command]), vec!["cargo-gamma", command]);
        }
    }

    #[test]
    fn a_misspelled_subcommand_is_left_for_clap_to_diagnose() {
        // Wrapping it in `run` would turn "did you mean `merge`?" into an unexpected-value error
        // about a subcommand the user did name.
        assert_eq!(normalize(["cargo-gamma", "gamma", "mrege"]), vec!["cargo-gamma", "mrege"]);
    }

    #[test]
    fn help_and_version_stay_at_the_top_level() {
        for flag in ["-h", "--help", "-V", "--version"] {
            assert_eq!(normalize(["cargo-gamma", "gamma", flag]), vec!["cargo-gamma", flag]);
        }
    }

    #[test]
    fn a_global_option_before_a_subcommand_is_stepped_over() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--color", "never", "merge", "a.json"]),
            vec!["cargo-gamma", "--color", "never", "merge", "a.json"]
        );
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--progress=never", "merge", "a.json"]),
            vec!["cargo-gamma", "--progress=never", "merge", "a.json"]
        );
    }

    #[test]
    fn a_global_option_before_no_subcommand_still_implies_run() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--color", "never", "--ops", "stmt"]),
            vec!["cargo-gamma", "run", "--color", "never", "--ops", "stmt"]
        );
    }

    #[test]
    fn a_dangling_global_option_does_not_panic() {
        assert_eq!(normalize(["cargo-gamma", "gamma", "--color"]), vec!["cargo-gamma", "run", "--color"]);
    }

    #[test]
    fn a_global_option_with_value_before_run_options_still_implies_run() {
        assert_eq!(
            normalize(["cargo-gamma", "gamma", "--progress=never", "--ops", "relational"]),
            vec!["cargo-gamma", "run", "--progress=never", "--ops", "relational"]
        );
    }
}
