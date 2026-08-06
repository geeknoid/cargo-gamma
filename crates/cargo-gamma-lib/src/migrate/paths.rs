//! Where the migration would read from and write to.

use camino::{Utf8Path, Utf8PathBuf};

use super::{SOURCE_PATH, TARGET_PATH};

/// Where the migration would read from and write to.
#[derive(Debug)]
pub struct Paths {
    /// The cargo-mutants configuration.
    pub source: Utf8PathBuf,

    /// The gamma configuration to write.
    pub target: Utf8PathBuf,
}

impl Paths {
    /// Resolves both paths against a project directory.
    #[must_use]
    pub fn resolve(dir: &Utf8Path) -> Self {
        Self {
            source: dir.join(SOURCE_PATH),
            target: dir.join(TARGET_PATH),
        }
    }
}
