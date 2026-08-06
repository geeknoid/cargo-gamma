/// What a directive asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Do not generate the named mutants here.
    Skip,

    /// The named mutants here are expected to survive; report it if they do not.
    ExpectMissed,

    /// The named mutants here are expected to be caught; report it if they are not.
    ExpectCaught,
}

impl Intent {
    /// Returns the attribute name that expresses this intent.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::ExpectMissed => "expect_missed",
            Self::ExpectCaught => "expect_caught",
        }
    }

    /// Resolves an attribute name to an intent.
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "skip" => Some(Self::Skip),
            "expect_missed" => Some(Self::ExpectMissed),
            "expect_caught" => Some(Self::ExpectCaught),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_names_round_trip_to_their_attribute_spellings() {
        assert_eq!(Intent::Skip.as_str(), "skip");
        assert_eq!(Intent::ExpectMissed.as_str(), "expect_missed");
        assert_eq!(Intent::ExpectCaught.as_str(), "expect_caught");
    }
}
