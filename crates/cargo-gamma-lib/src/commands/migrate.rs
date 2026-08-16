use std::fs;
use std::io::Write;

use crate::error::error;
use crate::report::{Styler, quantity};

use super::cli::MigrateArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;

/// Implements `migrate`.
pub(super) fn migrate<H: Host>(host: &mut H, args: &MigrateArgs, styler: Styler) -> crate::Result<i32> {
    if !args.command.is_empty() {
        return migrate_command(host, &args.command, styler);
    }

    let paths = crate::migrate::Paths::resolve(&args.dir);

    let text = fs::read_to_string(&paths.source).map_err(|cause| error!("could not read `{}`", paths.source).caused_by(cause).usage())?;

    let translation = crate::migrate::translate(&text)?;

    if args.dry_run {
        write!(host.results(), "{}", translation.text)?;
    } else {
        // Published rather than created and written into: the name must not be taken from someone
        // who has one already, and a write that fails part-way into a file created under that name
        // would leave a corrupt one there that the retry then refuses to overwrite.
        if !crate::elements::publish(&paths.target, &translation.text)? {
            // Whatever is already there took someone an afternoon, and there is no undo.
            return Err(error!(
                "{} already exists; move it aside or use --dry-run to see the translation",
                paths.target
            )
            .usage());
        }

        writeln!(host.error(), "{} {}", styler.verb("Wrote"), paths.target)?;
    }

    let mut stream = host.error();

    writeln!(
        stream,
        "{} {}: {} translated, {} settled, {} left as TODO",
        styler.verb("Migrated"),
        quantity(translation.total(), "key"),
        translation.translated,
        translation.settled.len(),
        translation.preserved.len()
    )?;

    // Said out loud rather than left in a comment, because the whole hazard is that a preserved key
    // used to suppress something and now suppresses nothing. Settled keys are deliberately not
    // named here: each already carries its reason in the file, and warning about a setting whose
    // behaviour gamma has anyway would send people looking for a problem that is not there.
    if !translation.preserved.is_empty() {
        writeln!(
            stream,
            "{} keys left as TODO are not recognised and currently do nothing; review them",
            styler.verb("Note")
        )?;
    }

    writeln!(
        stream,
        "{} .cargo/mutants.toml was left in place; delete it when you are satisfied",
        styler.verb("Note")
    )?;

    Ok(EXIT_OK)
}

/// Translates a foreign command line invocation.
fn migrate_command<H: Host>(host: &mut H, command: &[String], styler: Styler) -> crate::Result<i32> {
    let (translated, notes) = crate::migrate::translate_command(command);

    writeln!(host.results(), "{}", render_command(&translated))?;

    let mut stream = host.error();

    for note in &notes {
        writeln!(stream, "{} {note}", styler.verb("Note"))?;
    }

    Ok(EXIT_OK)
}

/// Renders already-tokenized arguments as one POSIX-shell-safe command line.
///
/// The migration input has already been separated into arguments, so joining it with spaces would
/// let a copied command change its own boundaries. This deliberately uses the small POSIX-safe
/// character set unquoted and single-quotes everything else, including an empty argument.
fn render_command(arguments: &[String]) -> String {
    arguments.iter().map(|argument| shell_word(argument)).collect::<Vec<_>>().join(" ")
}

/// Renders one argument as a POSIX shell word.
fn shell_word(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'))
    {
        return argument.to_owned();
    }

    format!("'{}'", argument.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::testing::{Sink, fails_at_every_line, workdir};

    #[test]
    fn dry_run_prints_translation_and_notes() {
        let dir = workdir("migrate-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(
            root.join(".cargo/mutants.toml"),
            "examine_globs = [\"src/**\"]\nsome_future_key = 1\n",
        )
        .expect("write source");

        let mut host = Sink::default();
        let code = migrate(
            &mut host,
            &MigrateArgs {
                dir: root.clone(),
                dry_run: true,
                command: Vec::new(),
            },
            Styler::new(false),
        )
        .expect("migrate");
        let out = String::from_utf8(host.out).expect("utf-8");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(out.contains("files = [\"src/**\"]"), "{out}");
        assert!(err.contains("keys left as TODO are not recognised"), "{err}");
        assert!(!root.join(".cargo/gamma.toml").exists());
    }

    #[test]
    fn migration_writes_when_not_a_dry_run_and_refuses_to_overwrite() {
        let dir = workdir("migrate-write-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "jobs = 2\n").expect("write source");

        let mut host = Sink::default();
        let args = MigrateArgs {
            dir: root.clone(),
            dry_run: false,
            command: Vec::new(),
        };

        assert_eq!(migrate(&mut host, &args, Styler::new(false)).expect("migrate"), EXIT_OK);
        assert!(root.join(".cargo/gamma.toml").exists());
        fs::write(root.join(".cargo/gamma.toml"), "hand written = true\n").expect("replace target");
        assert!(migrate(&mut host, &args, Styler::new(false)).unwrap_err().is_usage());
        assert_eq!(
            fs::read_to_string(root.join(".cargo/gamma.toml")).expect("read target"),
            "hand written = true\n"
        );
    }

    #[test]
    fn command_translation_prints_notes_for_gaps() {
        let mut host = Sink::default();

        let code = migrate(
            &mut host,
            &MigrateArgs {
                dir: Utf8PathBuf::from("."),
                dry_run: false,
                command: vec![
                    "cargo".to_owned(),
                    "mutants".to_owned(),
                    "--shuffle".to_owned(),
                    "--unknown".to_owned(),
                ],
            },
            Styler::new(false),
        )
        .expect("migrate command");
        let out = String::from_utf8(host.out).expect("utf-8");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(out.starts_with("cargo gamma run"), "{out}");
        assert!(err.contains("dropped"), "{err}");
        assert!(err.contains("no gamma equivalent"), "{err}");
    }

    #[test]
    fn migrated_commands_use_posix_shell_safe_words() {
        let rendered = render_command(
            &[
                "cargo",
                "gamma",
                "run",
                "two words",
                "quote'and\"double",
                "*.rs",
                "$HOME; :",
                "",
                "after-empty",
            ]
            .map(str::to_owned),
        );

        assert_eq!(
            rendered,
            "cargo gamma run 'two words' 'quote'\"'\"'and\"double' '*.rs' '$HOME; :' '' after-empty"
        );
    }

    /// The advertised output is documented as POSIX-shell-safe, so let a POSIX shell read it
    /// back. Empty sits before another word so line-oriented output retains it unambiguously.
    #[cfg(unix)]
    #[test]
    fn shell_safe_migrated_commands_round_trip_spaced_quoted_glob_metacharacter_and_empty_arguments() {
        let arguments = [
            "cargo",
            "gamma",
            "run",
            "two words",
            "quote'and\"double",
            "*.rs",
            "$HOME; :",
            "",
            "after-empty",
        ]
        .map(str::to_owned);
        let rendered = render_command(&arguments);
        let script = format!("set -- {rendered}; printf '%s\\n' \"$@\"");
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .expect("POSIX shell");

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

        let stdout = String::from_utf8(output.stdout).expect("shell output");
        let actual: Vec<&str> = stdout.strip_suffix('\n').expect("one newline per argument").split('\n').collect();

        assert_eq!(actual, arguments.iter().map(String::as_str).collect::<Vec<_>>());
    }

    /// A closed stream has to surface from the config migration rather than half-report.
    #[test]
    fn a_closed_stream_is_reported_by_the_config_migration() {
        let dir = workdir("migrate-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(
            root.join(".cargo/mutants.toml"),
            "examine_globs = [\"src/**\"]\nsome_future_key = 1\n",
        )
        .expect("source");

        // Each attempt gets its own directory, because a successful attempt writes the target and
        // the next one would then trip over it instead of over the closed stream.
        fails_at_every_line(4, |host| {
            let attempt = workdir("migrate-broken-attempt-");
            let at = Utf8PathBuf::from_path_buf(attempt.path().to_path_buf()).expect("utf8");
            fs::create_dir_all(at.join(".cargo")).expect("cargo dir");
            let _bytes = fs::copy(root.join(".cargo/mutants.toml"), at.join(".cargo/mutants.toml")).expect("copy");

            let args = MigrateArgs {
                dir: at,
                dry_run: false,
                command: Vec::new(),
            };

            migrate(host, &args, Styler::new(false)).map(|_| ())
        });
    }

    /// And likewise from the command migration.
    #[test]
    fn a_closed_stream_is_reported_by_the_command_migration() {
        let args = MigrateArgs {
            dir: Utf8PathBuf::from("."),
            dry_run: false,
            command: vec!["cargo".to_owned(), "mutants".to_owned(), "--shuffle".to_owned()],
        };

        fails_at_every_line(2, |host| migrate(host, &args, Styler::new(false)).map(|_| ()));
    }

    /// A migration that cannot write leaves nothing under the final name.
    ///
    /// The target is created no-clobber, so anything left there by a failure would block every
    /// retry forever — the user would be told the file already exists, and the file that exists
    /// would be the wreckage of the attempt that failed. Blocking the staging file stands in for
    /// the disk filling up part-way through the write.
    #[test]
    fn a_failed_write_leaves_no_file_to_block_the_retry() {
        let dir = workdir("migrate-failed-write-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "jobs = 2\n").expect("write source");

        let target = root.join(".cargo/gamma.toml");
        let scratch = crate::elements::scratch_path(&target);

        fs::create_dir(scratch.as_std_path()).expect("block the staging file");
        crate::elements::next_scratch_path(scratch.clone());

        let args = MigrateArgs {
            dir: root,
            dry_run: false,
            command: Vec::new(),
        };
        let mut host = Sink::default();

        let error = migrate(&mut host, &args, Styler::new(false)).expect_err("the write must fail");

        assert!(!error.is_usage(), "a failed write is not the user's mistake: {error}");
        assert!(!target.as_std_path().exists(), "a failed migration left the target taken");

        // And with the obstruction gone the migration runs, which is what "no blocking target"
        // is worth: the retry is the ordinary command, not a cleanup task.
        fs::remove_dir(scratch.as_std_path()).expect("clear the obstruction");
        assert_eq!(migrate(&mut host, &args, Styler::new(false)).expect("retry"), EXIT_OK);
        assert!(target.as_std_path().exists());
    }

    /// A missing source config is the user's mistake, not the tool's.
    #[test]
    fn a_missing_source_config_is_a_usage_error() {
        let dir = workdir("migrate-missing-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        let args = MigrateArgs {
            dir: root,
            dry_run: false,
            command: Vec::new(),
        };
        let mut host = Sink::default();

        let error = migrate(&mut host, &args, Styler::new(false)).expect_err("missing source");

        assert!(error.is_usage(), "{error}");
    }
}
