//! Porting a project from another mutation tester, once, reviewably.
//!
//! Gamma deliberately reads neither the foreign configuration file nor the flags that go with it:
//! the two catalogs are different, so honouring foreign suppressions silently would mean those
//! settings quietly changing which mutants this one skips, and therefore quietly changing the
//! score. Migration is the alternative to that silence.
//!
//! The rules the translator holds itself to:
//!
//! **Nothing is ever dropped.** A key this tool cannot express becomes a `TODO` comment carrying its
//! rendered TOML value. The value is preserved semantically; source comments and formatting are not.
//! A migration that silently discards a setting is exactly the failure mode the whole design is
//! trying to avoid.
//!
//! **Every line says where it came from.** The output is meant to be read by someone who knows the
//! old configuration and not the new one, so each translated line is annotated with the key it
//! replaces.
//!
//! **Nothing is overwritten.** An existing `gamma.toml` stops the migration rather than being
//! replaced, because the thing being overwritten is the thing that took someone an afternoon.

mod command;
mod config;
mod lock_flags;
mod paths;
mod translation;

pub use command::translate_command;
pub use config::translate;
pub(crate) use lock_flags::{adjust_lock_flags, lock_flag_reason};
pub use paths::Paths;
pub use translation::Translation;
