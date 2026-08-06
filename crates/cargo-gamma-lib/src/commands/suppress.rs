use std::collections::BTreeSet;
use std::fs;
use std::io::Write;

use camino::Utf8PathBuf;

use crate::discover::Plan;
use crate::error::error;
use crate::report::{Styler, quantity};

use super::cli::SuppressArgs;
use super::dispatch::EXIT_OK;
use super::host::Host;
use super::run::execute;
use super::when::When;

/// Implements `suppress`.
///
/// The order is deliberate: run first, then write. Suppressions are derived from observed verdicts,
/// never from static guesses, because the whole justification for editing someone's source is that
/// the tool watched the mutant misbehave.
pub(super) fn suppress<H: Host>(host: &mut H, args: &SuppressArgs, progress_when: When, styler: Styler) -> crate::Result<i32> {
    let eligible = crate::fix::Eligible::parse(&args.eligible)?;

    if eligible.is_empty() {
        return Err(error!("--eligible named no verdicts; nothing could be suppressed").usage());
    }

    let Some(plan) = execute(host, &args.run, progress_when, styler)? else {
        return Ok(EXIT_OK);
    };

    let edits = crate::fix::plan(&plan.mutants, &eligible);

    if edits.is_empty() {
        writeln!(
            host.error(),
            "{} nothing to suppress: no mutant had an eligible verdict",
            styler.verb("Finished")
        )?;

        return Ok(EXIT_OK);
    }

    // A set rather than a scan of the edits for every mutant: both grow with the size of the
    // workspace, and the pairing has no business being quadratic in it.
    let touched: crate::HashSet<(&camino::Utf8Path, usize)> =
        edits.iter().map(|edit| (edit.file.as_path(), edit.line)).collect();

    let intended: BTreeSet<String> = plan
        .mutants
        .iter()
        .filter(|mutant| touched.contains(&(mutant.file.as_path(), mutant.line)))
        .map(|mutant| mutant.id.clone())
        .collect();

    let date = crate::fix::today();
    let mut written = Vec::new();

    for file in &plan.files {
        let for_file: Vec<&crate::fix::Edit> = edits.iter().filter(|edit| edit.file == file.path).collect();

        if for_file.is_empty() {
            continue;
        }

        let path = plan.root.join(&file.path);
        let before = fs::read_to_string(&path)
            .map_err(|cause| error!("could not read {path}").caused_by(cause))?;
        let after = crate::fix::apply(&before, &for_file, &date);

        // Parsing before writing, not after: a patch that does not parse must never reach the disk,
        // because the revert path is only as good as the copy it holds.
        let _ = syn::parse_file(&after)
            .map_err(|cause| error!("the generated directive would not parse in {path}").caused_by(cause))?;

        if args.dry_run_suppress {
            write!(host.output(), "{}", crate::fix::diff(&file.path, &before, &after))?;
        } else {
            fs::write(&path, &after).map_err(|cause| error!("could not write {path}").caused_by(cause))?;
            written.push((path, before));
        }
    }

    if args.dry_run_suppress {
        return Ok(EXIT_OK);
    }

    verify_or_revert(host, args, &plan, &intended, written, styler)
}

/// Re-runs discovery over the edited tree and reverts unless the suppressed set is exactly right.
///
/// Over-suppression is the hazard: a directive attached to a multi-line construct silently takes out
/// everything inside it, which can include survivors. Checking both directions is what makes an
/// automated source edit something a reviewer can trust without reading every line of it.
fn verify_or_revert<H: Host>(
    host: &mut H,
    args: &SuppressArgs,
    before: &Plan,
    intended: &BTreeSet<String>,
    written: Vec<(Utf8PathBuf, String)>,
    styler: Styler,
) -> crate::Result<i32> {
    let selection = args.run.select.selection()?;
    let after = crate::discover::plan(&args.run.select, &selection, args.run.select.shard()?, &mut |_| {})?;
    let result = crate::fix::verify(&before.mutants, &after.mutants, intended);

    if result.is_clean() {
        let mut stream = host.error();

        writeln!(
            stream,
            "{} {} in {}",
            styler.verb("Suppressed"),
            quantity(intended.len(), "directive"),
            quantity(written.len(), "file")
        )?;

        writeln!(
            stream,
            "{} every generated directive is tagged; grep for `cargo gamma suppress` to audit them",
            styler.verb("Note")
        )?;

        return Ok(EXIT_OK);
    }

    for (path, original) in written {
        fs::write(&path, original).map_err(|cause| error!("could not revert {path}").caused_by(cause))?;
    }

    Err(error!(
        "the generated directives did not suppress what they were meant to ({} missing, {} unintended); every edit has been reverted",
        result.missing.len(),
        result.collateral.len()
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use crate::commands::RunArgs;
    use crate::discover::TargetFile;

    use super::*;
    use crate::testing::{Sink, fails_at_every_line, workdir};

    fn crate_dir(name: &str) -> tempfile::TempDir {
        let dir = workdir(name);
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        fs::create_dir(root.join("src")).expect("src");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("manifest");
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 42 }\n").expect("lib");

        dir
    }

    /// Builds a `suppress` invocation that only discovers, so no cargo build is involved.
    fn dry_args(root: &Utf8PathBuf, eligible: &str) -> SuppressArgs {
        SuppressArgs {
            run: RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                dry_run: true,
                ..RunArgs::default()
            },
            dry_run_suppress: false,
            eligible: eligible.to_owned(),
        }
    }

    #[test]
    fn empty_eligibility_is_a_usage_error() {
        let mut host = Sink::default();
        let args = SuppressArgs {
            run: RunArgs::default(),
            dry_run_suppress: false,
            eligible: String::new(),
        };

        let err = suppress(&mut host, &args, When::Never, Styler::new(false)).unwrap_err();

        assert!(err.is_usage());
    }

    #[test]
    fn clean_verification_reports_what_was_suppressed() {
        let dir = crate_dir("suppress-verify-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = SuppressArgs {
            run: RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                ..RunArgs::default()
            },
            dry_run_suppress: false,
            eligible: "timeout".to_owned(),
        };
        let before = Plan {
            root: root.clone(),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute: root.join("src/lib.rs"),
                package: "subject".to_owned(),
            }],
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let mut host = Sink::default();

        let code = verify_or_revert(
            &mut host,
            &args,
            &before,
            &BTreeSet::new(),
            Vec::new(),
            Styler::new(false),
        )
        .expect("verify");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(err.contains("Suppressed 0 directives"), "{err}");
        assert!(err.contains("grep for `cargo gamma suppress`"), "{err}");
        assert!(host.out.is_empty());
    }

    /// A run that produced no mutants leaves nothing to suppress and is not a failure.
    #[test]
    fn a_run_with_no_mutants_at_all_succeeds_quietly() {
        let dir = crate_dir("suppress-none-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::write(root.join("src/lib.rs"), "pub struct Empty;\n").expect("lib");

        let mut host = Sink::default();

        let code = suppress(&mut host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).expect("suppress");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("no mutants were generated"), "{}", host.err());
    }

    /// Mutants that were never run have no eligible verdict, so nothing gets edited.
    #[test]
    fn a_population_with_no_eligible_verdict_edits_nothing() {
        let dir = crate_dir("suppress-ineligible-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let source = fs::read_to_string(root.join("src/lib.rs")).expect("read");

        let mut host = Sink::default();

        let code = suppress(&mut host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).expect("suppress");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("nothing to suppress"), "{}", host.err());
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).expect("read"), source);

        fails_at_every_line(1, |host| {
            suppress(host, &dry_args(&root, "timeout"), When::Never, Styler::new(false)).map(|_| ())
        });
    }

    /// Directives that missed their target take the whole edit back rather than leaving it half done.
    #[test]
    fn a_verification_that_fails_reverts_every_file_it_wrote() {
        let dir = crate_dir("suppress-revert-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("src/lib.rs");
        let original = fs::read_to_string(&path).expect("read");

        fs::write(&path, "pub fn answer() -> i32 { 0 }\n").expect("edited");

        let before = Plan {
            root: root.clone(),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute: path.clone(),
                package: "subject".to_owned(),
            }],
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };

        // Naming an id that no mutant carries makes the intended set impossible to satisfy, which
        // is exactly the shape of an over- or under-reaching directive.
        let intended: BTreeSet<String> = core::iter::once("never-generated".to_owned()).collect();
        let mut host = Sink::default();

        let error = verify_or_revert(
            &mut host,
            &dry_args(&root, "timeout"),
            &before,
            &intended,
            vec![(path.clone(), original.clone())],
            Styler::new(false),
        )
        .expect_err("verification should fail");

        assert!(error.to_string().contains("every edit has been reverted"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("read"), original);
    }

    /// A closed stream has to surface from the success report.
    #[test]
    fn a_closed_stream_is_reported_by_the_success_report() {
        let dir = crate_dir("suppress-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let before = Plan {
            root: root.clone(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };

        fails_at_every_line(2, |host| {
            verify_or_revert(
                host,
                &dry_args(&root, "timeout"),
                &before,
                &BTreeSet::new(),
                Vec::new(),
                Styler::new(false),
            )
            .map(|_| ())
        });
    }
}
