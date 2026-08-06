use crate::Result;
use crate::error::error;
use crate::model::Outcome;

/// A verdict that may be suppressed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Eligible {
    /// The mutant exceeded its time budget.
    ///
    /// The default, and still the second-best answer: a timeout that is cached keeps the mutant in
    /// the score and costs nothing on re-runs, whereas suppressing it removes it from the
    /// denominator. This is for sites that are permanently un-mutatable — a hand-written spin loop,
    /// a driver poll, a reactor — where the team wants that recorded where the next reader sees it.
    Timeout,

    /// The mutant did not compile.
    Unviable,
}

impl Eligible {
    /// Parses the `--eligible` list.
    ///
    /// `missed` and `survived` are named explicitly so the refusal can explain itself. Falling
    /// through to "unknown verdict" would read like a typo, and the person who typed it would try
    /// harder rather than reconsidering.
    pub fn parse(list: &str) -> Result<Vec<Self>> {
        let mut out = Vec::new();

        for entry in list.split(',').map(str::trim).filter(|entry| !entry.is_empty()) {
            match entry {
                "timeout" => out.push(Self::Timeout),
                "unviable" | "compile-error" => out.push(Self::Unviable),

                "missed" | "survived" | "survivor" => {
                    return Err(error!(
                        "`{entry}` is not eligible for `suppress`, and cannot be made eligible: a surviving mutant is a gap in the test suite, and suppressing it would remove that gap from the score rather than from the code"
                    )
                    .usage());
                }

                other => {
                    return Err(error!("unknown verdict `{other}`; --eligible accepts `timeout` and `unviable`").usage());
                }
            }
        }

        out.sort_unstable();
        out.dedup();

        Ok(out)
    }

    /// The tag written into generated directives.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unviable => "unviable",
        }
    }

    /// Returns the verdict this covers.
    #[must_use]
    pub const fn outcome(self) -> Outcome {
        match self {
            Self::Timeout => Outcome::Timeout,
            Self::Unviable => Outcome::CompileError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_survivor_cannot_be_made_eligible() {
        // The single most important test in the module. If this ever passes, every mutation score
        // the tool reports becomes a number that can be improved by editing comments.
        let cause = Eligible::parse("missed").expect_err("survivors must be refused");

        assert!(cause.is_usage(), "{cause}");
        assert!(cause.to_string().contains("gap in the test suite"), "{cause}");
    }

    #[test]
    fn eligible_lists_are_trimmed_sorted_and_deduplicated() {
        let parsed = Eligible::parse(" unviable, timeout, compile-error, timeout ").unwrap();

        assert_eq!(parsed, vec![Eligible::Timeout, Eligible::Unviable]);
    }

    #[test]
    fn unknown_eligible_verdicts_are_errors() {
        let cause = Eligible::parse("killed").expect_err("unknown verdict should be rejected");

        assert!(cause.is_usage(), "{cause}");
        assert!(cause.to_string().contains("timeout"), "{cause}");
    }
}
