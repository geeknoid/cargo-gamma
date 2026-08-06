//! Turning source text into a syntax tree with byte-accurate spans and comment trivia.
//!
//! Two things make this module more than a thin wrapper around `syn::parse_file`.
//!
//! The first is byte accuracy. Instrumentation is a text splice, not a token-tree rewrite, because
//! re-emitting a `syn` tree through `quote` would reformat every file it touches and destroy the
//! line numbers that every report, every suppression and every diff depends on. That requires
//! exact byte offsets for each node, which `proc-macro2` provides only with the `span-locations`
//! feature enabled.
//!
//! The second is comments. Comments are trivia: they are not in the syntax tree at all, so a
//! suppression written as a comment is invisible to `syn`. This module scans the raw text for
//! them, which means it needs a small but honest lexer that knows about raw strings, escapes and
//! the lifetime-versus-character-literal ambiguity.

mod comment;
mod source_file;

pub use comment::{Comment, CommentKind};
pub use source_file::SourceFile;
