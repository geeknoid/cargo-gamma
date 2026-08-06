//! How much CI surfacing to emit.

use clap::ValueEnum;

/// How much of the CI surfacing to emit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Annotations {
    /// Emit nothing.
    None,

    /// Emit the GitHub renderings when running inside GitHub Actions, and nothing otherwise.
    #[default]
    Auto,

    /// Emit the GitHub renderings regardless of where we are running.
    Github,
}
