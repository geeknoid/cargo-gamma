use clap::{ColorChoice, ValueEnum};

/// When to colorize output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum When {
    /// Colorize when the stream is a terminal.
    #[default]
    Auto,

    /// Never colorize.
    Never,

    /// Always colorize.
    Always,
}

impl When {
    /// Resolves the setting against whether the stream is actually a terminal.
    #[must_use]
    pub const fn resolve(self, is_terminal: bool) -> bool {
        match self {
            Self::Auto => is_terminal,
            Self::Never => false,
            Self::Always => true,
        }
    }

    /// Converts to clap's equivalent, for help rendering.
    #[must_use]
    pub const fn to_clap(self) -> ColorChoice {
        match self {
            Self::Auto => ColorChoice::Auto,
            Self::Never => ColorChoice::Never,
            Self::Always => ColorChoice::Always,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_auto_follows_the_terminal() {
        assert!(When::Auto.resolve(true));
        assert!(!When::Auto.resolve(false));
        assert!(!When::Never.resolve(true));
        assert!(When::Always.resolve(false));
    }

    #[test]
    fn when_maps_onto_claps_choice() {
        assert_eq!(When::Auto.to_clap(), ColorChoice::Auto);
        assert_eq!(When::Never.to_clap(), ColorChoice::Never);
        assert_eq!(When::Always.to_clap(), ColorChoice::Always);
    }
}
