//! The cargo flags that forbid a lockfile update, and what gamma runs in their place.
//!
//! This lives beside the migration rather than beside the build because both need the same answer:
//! a migrated configuration that carried `--locked` across verbatim would produce a gamma
//! configuration that cannot run, which is the same failure the run itself has to avoid.

/// The cargo flags that promise the lockfile will not be written, and which gamma therefore cannot
/// honour.
///
/// `--frozen` is `--locked` and `--offline` together, so both land here and both become the half
/// gamma can keep.
const LOCK_FLAGS: [&str; 2] = ["--locked", "--frozen"];

/// What gamma runs in their place.
const LOCK_SUBSTITUTE: &str = "--offline";

/// Returns what gamma runs instead of a flag that forbids updating the lockfile.
///
/// This lives beside the migration rather than beside the build because both need the same answer:
/// a migrated configuration that carried `--locked` across verbatim would produce a gamma
/// configuration that cannot run, which is the same failure the run itself has to avoid.
pub(super) fn substitute_lock_flag(arg: &str) -> Option<&'static str> {
    LOCK_FLAGS.contains(&arg).then_some(LOCK_SUBSTITUTE)
}

/// Says which flags were adjusted and why, in the one sentence that has to carry it.
///
/// The reason is the useful half: `--locked` is not a whim, so a reader who sees it dropped needs
/// to know that gamma writes a dependency into the manifest before it builds and that the part of
/// the promise that can be kept — no network, no version drift — still is.
pub(crate) fn lock_flag_reason(flags: &[String]) -> String {
    let named = flags.join("`, `");

    format!(
        "`{named}` adjusted to `{LOCK_SUBSTITUTE}`: gamma adds a `{runtime}` dependency to the workspace \
         manifest before it builds, so the lockfile has to be updated and a frozen lockfile is not a \
         constraint gamma can meet. `{LOCK_SUBSTITUTE}` keeps the part that can be kept: no network \
         access and no new versions",
        runtime = crate::exec::RUNTIME_CRATE
    )
}

/// Rewrites the flags that forbid a lockfile update, reporting which ones were rewritten.
///
/// A flag already spelled `--offline`, or a second lock flag on the same command line, must not
/// produce a second `--offline`: cargo accepts the repetition, but the reader of a migrated config
/// would reasonably wonder what the duplicate meant.
pub(crate) fn adjust_lock_flags(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut adjusted: Vec<String> = Vec::with_capacity(args.len());
    let mut substituted = Vec::new();

    for arg in args {
        let Some(replacement) = substitute_lock_flag(arg) else {
            // A substitute the user wrote themselves counts as one already present, so a run that
            // asked for both does not end up saying `--offline` twice.
            if arg != LOCK_SUBSTITUTE || !adjusted.iter().any(|kept| kept == LOCK_SUBSTITUTE) {
                adjusted.push(arg.clone());
            }

            continue;
        };

        substituted.push(arg.clone());

        if !adjusted.iter().any(|kept| kept == replacement) {
            adjusted.push(replacement.to_owned());
        }
    }

    (adjusted, substituted)
}

#[cfg(test)]
mod tests {
    use super::super::config::translate;
    use super::*;

    /// A promise gamma cannot keep is worse than one it never made: `--locked` forbids the
    /// lockfile update that adding the guard runtime forces, so a config that carried it across
    /// verbatim failed on its first build.
    #[test]
    fn a_lockfile_promise_becomes_the_half_of_it_that_can_be_kept() {
        let out = translate("additional_cargo_args = [\"--locked\", \"-v\"]\n").expect("translates");

        assert!(out.text.contains("cargo-args = [\"--offline\", \"-v\"]"), "{}", out.text);
        assert!(!out.text.contains("\"--locked\""), "{}", out.text);
    }

    /// The substitution has to say what it did and why, or the next reader goes looking for the
    /// flag they wrote and finds a different one with no explanation.
    #[test]
    fn the_substitution_warns_and_says_why() {
        let out = translate("additional_cargo_args = [\"--locked\"]\n").expect("translates");

        assert!(out.text.contains("`--locked` adjusted to `--offline`"), "{}", out.text);
        assert!(
            out.text.contains("gamma_rt"),
            "the reason must name what forces the lockfile update: {}",
            out.text
        );

        // Whatever the note says, the file it is written into still has to load.
        let config = crate::config::Config::parse(&out.text).expect("the generated file must load");

        assert_eq!(config.cargo_args, vec!["--offline".to_owned()]);
    }

    /// `--frozen` is `--locked` and `--offline` at once. The half gamma cannot keep goes; the half
    /// it can is exactly what the substitute already is.
    #[test]
    fn frozen_is_treated_the_same_as_locked() {
        let out = translate("additional_cargo_args = [\"--frozen\"]\n").expect("translates");

        assert!(out.text.contains("cargo-args = [\"--offline\"]"), "{}", out.text);
        assert!(out.text.contains("`--frozen` adjusted"), "{}", out.text);
    }

    /// Two flags meaning the same thing must not leave two copies of the substitute behind, which
    /// would only raise the question of what the second one was for.
    #[test]
    fn a_repeated_lockfile_promise_leaves_one_substitute() {
        let (adjusted, substituted) = adjust_lock_flags(&["--locked".to_owned(), "--frozen".to_owned(), "--offline".to_owned()]);

        assert_eq!(adjusted, vec!["--offline".to_owned()]);
        assert_eq!(substituted, vec!["--locked".to_owned(), "--frozen".to_owned()]);
    }
}
