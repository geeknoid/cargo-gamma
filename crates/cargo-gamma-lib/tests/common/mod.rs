//! A `Host` that captures everything the tool writes and reports a fixed terminal shape.
//!
//! This is what the thin-binary structure buys. Every exit code, every stream-routing decision and
//! every terminal-dependent branch becomes an ordinary assertion here, instead of something
//! verified by running the binary and reading the output by eye.

#![allow(dead_code, reason = "the module is shared by several test binaries, each using a subset")]

use cargo_gamma_lib::Host;
use std::io::Write;

/// Captures both output streams.
#[derive(Debug, Default)]
pub struct FakeHost {
    /// Everything written to the output stream.
    pub out: Vec<u8>,

    /// Everything written to the diagnostic stream.
    pub err: Vec<u8>,

    /// What [`Host::is_terminal`] should report.
    pub terminal: bool,

    /// What [`Host::terminal_width`] should report.
    pub width: Option<u16>,

    /// What [`Host::env`] should report, instead of the real environment.
    ///
    /// A test that wants to look like a CI runner must not set a real variable: the whole suite
    /// shares one process, so it would be setting it for every other test too.
    pub environment: Vec<(String, String)>,
}

impl FakeHost {
    /// A host that looks like a pipe: no terminal, no width.
    #[must_use]
    pub fn piped() -> Self {
        Self::default()
    }

    /// A host that looks like a terminal of the given width.
    #[must_use]
    pub fn terminal(width: u16) -> Self {
        Self {
            terminal: true,
            width: Some(width),
            ..Self::default()
        }
    }

    /// Pretends the given environment variable is set.
    #[must_use]
    pub fn with_env(mut self, name: &str, value: &str) -> Self {
        self.environment.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The captured output stream as text.
    #[must_use]
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    /// The captured diagnostic stream as text.
    #[must_use]
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.err).into_owned()
    }
}

impl Host for FakeHost {
    fn output(&mut self) -> impl Write {
        &mut self.out
    }

    fn error(&mut self) -> impl Write {
        &mut self.err
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn terminal_width(&self) -> Option<u16> {
        self.width
    }

    fn env(&self, name: &str) -> Option<String> {
        self.environment.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
    }
}
