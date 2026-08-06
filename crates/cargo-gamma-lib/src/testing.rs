//! Shared fakes for the unit tests.
//!
//! Nearly every command is a function of a [`Host`], so nearly every test needs one. Defining a
//! capturing host once means the stream-plumbing is written and exercised in exactly one place
//! instead of being copied, subtly differently, into a dozen `mod tests` blocks.

use core::cell::Cell;
use std::io::{self, Write};

use crate::commands::Host;

/// A [`Host`] that captures both streams in memory.
///
/// The defaults describe a plain redirected pipe: not a terminal, no width, and an empty
/// environment. [`Sink::terminal`] and [`Sink::with_env`] override those for the tests that care.
#[derive(Default)]
pub struct Sink {
    /// Everything written to the result stream.
    pub out: Vec<u8>,

    /// Everything written to the diagnostic stream.
    pub err: Vec<u8>,

    terminal: bool,
    width: Option<u16>,
    env: Vec<(String, String)>,
}

impl Sink {
    /// Presents the host as a terminal of the given width.
    #[must_use]
    pub fn terminal(mut self, width: u16) -> Self {
        self.terminal = true;
        self.width = Some(width);
        self
    }

    /// Adds a variable to the fake environment.
    #[must_use]
    pub fn with_env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The captured result stream, as text.
    #[must_use]
    pub fn out(&self) -> String {
        String::from_utf8(self.out.clone()).expect("output should be utf-8")
    }

    /// The captured diagnostic stream, as text.
    #[must_use]
    pub fn err(&self) -> String {
        String::from_utf8(self.err.clone()).expect("diagnostics should be utf-8")
    }
}

impl Host for Sink {
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
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
    }
}

/// A writer that refuses every write.
///
/// This stands in for the closed pipe you get when the user pipes the tool into `head`, which is
/// the only way the `?` on a `writeln!` to the console ever fires in practice.
pub struct Broken;

impl Write for Broken {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

/// A [`Host`] whose streams are both closed pipes.
pub struct BrokenHost;
impl Host for BrokenHost {
    fn output(&mut self) -> impl Write {
        Broken
    }

    fn error(&mut self) -> impl Write {
        Broken
    }

    fn is_terminal(&self) -> bool {
        false
    }

    fn terminal_width(&self) -> Option<u16> {
        None
    }

    fn env(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Wraps a scratch directory as a workspace whose test binaries are `/bin/sh` running `body`.
///
/// The real spawn, drain, wait and kill machinery is exercised; only the compiled test harness is
/// stood in for. The script is passed as an argument rather than written to disk, because a file
/// made executable while other threads are forking can be refused with `ETXTBSY`, which would make
/// such tests fail intermittently for a reason unrelated to what they assert.
#[cfg(unix)]
pub fn shell_workspace(prefix: &str, body: &str) -> (tempfile::TempDir, crate::exec::Workspace) {
    let directory = workdir(prefix);
    let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
    let target = root.join("target");
    let mut work = crate::exec::Workspace::adopt(root, target);

    work.set_test_args(vec!["-c".to_owned(), body.to_owned()]);

    (directory, work)
}

/// A [`TestBinary`](crate::exec::TestBinary) at `path`, with everything else left at its default.
///
/// Most tests care about one field — where the executable is, or which package it belongs to — and
/// spelling out the rest at each site makes adding a field a sweep through the whole suite.
pub fn test_binary(path: &str) -> crate::exec::TestBinary {
    crate::exec::TestBinary {
        path: camino::Utf8PathBuf::from(path),
        package: String::new(),
        target: String::new(),
        manifest_dir: camino::Utf8PathBuf::new(),
        baseline: core::time::Duration::ZERO,
        budget: core::time::Duration::ZERO,
        peak: None,
        memory: None,
    }
}

/// Records every event a run publishes, so a test can assert on what it announced.
///
/// The console implementations format and discard; this keeps the structure, which is what an
/// assertion about "did this phase run" actually needs.
#[derive(Default)]
pub struct Recorder {
    /// Every `(verb, detail)` pair, in the order it was published.
    pub phases: Vec<(String, String)>,

    /// How many mutants were announced.
    pub mutants: usize,
}

impl crate::exec::Events for Recorder {
    fn phase(&mut self, verb: &str, detail: &str) {
        self.phases.push((verb.to_owned(), detail.to_owned()));
    }

    fn mutant(&mut self, _mutant: &crate::model::Mutant) {
        self.mutants = self.mutants.saturating_add(1);
    }
}

/// Creates a temporary directory under the workspace target directory.
///
/// Tests that shell out to cargo need their scratch space on the same file system as the target
/// directory, and keeping it there also means `cargo clean` sweeps up anything a panicking test
/// leaked.
#[must_use]
pub fn workdir(prefix: &str) -> tempfile::TempDir {
    let work = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work");

    std::fs::create_dir_all(&work).expect("the test work directory should be creatable");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(work)
        .expect("the temporary directory should be creatable")
}

/// A writer that accepts a fixed number of lines and then behaves like a closed pipe.
///
/// Lines rather than writes, because `writeln!` turns one call into several `write` calls and a
/// test that counted those would be asserting on the internals of `format_args!`.
pub struct Flaky<'a> {
    remaining: &'a Cell<usize>,
}

impl Write for Flaky<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.remaining.get() == 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }

        #[expect(clippy::naive_bytecount, reason = "a test double is not worth a dependency")]
        let lines = buf.iter().filter(|byte| **byte == b'\n').count();

        self.remaining.set(self.remaining.get().saturating_sub(lines));

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`Host`] whose streams close after a given number of lines.
///
/// Console output is written a line at a time, and a host that fails on the very first one only
/// ever proves the first `?` works. Moving the failure along one line at a time is what reaches
/// the rest of them.
pub struct FlakyHost {
    remaining: Cell<usize>,
}

impl FlakyHost {
    /// Creates a host that accepts `lines` lines on each stream before failing.
    #[must_use]
    pub const fn new(lines: usize) -> Self {
        Self {
            remaining: Cell::new(lines),
        }
    }
}

impl Host for FlakyHost {
    fn output(&mut self) -> impl Write {
        Flaky {
            remaining: &self.remaining,
        }
    }

    fn error(&mut self) -> impl Write {
        Flaky {
            remaining: &self.remaining,
        }
    }

    fn is_terminal(&self) -> bool {
        false
    }

    fn terminal_width(&self) -> Option<u16> {
        None
    }

    fn env(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Asserts that `run` reports a closed pipe however many lines it managed to write first.
///
/// Every console line is a place the pipe can go away, and a `?` that is never taken is a `?` that
/// has never been shown to propagate rather than swallow.
pub fn fails_at_every_line<E: core::fmt::Display>(lines: usize, run: impl Fn(&mut FlakyHost) -> Result<(), E>) {
    for limit in 0..lines {
        let mut host = FlakyHost::new(limit);

        let error = run(&mut host).err().unwrap_or_else(|| panic!("line {limit} should have failed"));

        assert!(error.to_string().contains("broken pipe"), "line {limit}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A default sink looks like a redirected pipe and captures both streams.
    #[test]
    fn a_default_sink_captures_both_streams_and_reports_no_terminal() {
        let mut sink = Sink::default();

        write!(sink.output(), "result").expect("write");
        write!(sink.error(), "progress").expect("write");

        assert_eq!(sink.out(), "result");
        assert_eq!(sink.err(), "progress");
        assert!(!sink.is_terminal());
        assert_eq!(sink.terminal_width(), None);
        assert_eq!(sink.env("GAMMA_NOT_SET"), None);
    }

    /// The builders override the terminal and environment answers.
    #[test]
    fn the_builders_override_the_terminal_and_the_environment() {
        let sink = Sink::default().terminal(80).with_env("CI", "true");

        assert!(sink.is_terminal());
        assert_eq!(sink.terminal_width(), Some(80));
        assert_eq!(sink.env("CI").as_deref(), Some("true"));
        assert_eq!(sink.env("OTHER"), None);
    }

    /// Every stream of a broken host fails, on both write and flush.
    #[test]
    fn a_broken_host_fails_every_write_and_every_flush() {
        let mut host = BrokenHost;

        assert_eq!(host.output().write(b"x").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(host.output().flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(host.error().write(b"x").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(host.error().flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
        assert_eq!(host.env("PATH"), None);
    }

    /// The flaky host lets exactly the budgeted number of lines through.
    #[test]
    fn a_flaky_host_fails_once_its_line_budget_is_spent() {
        let mut host = FlakyHost::new(2);

        writeln!(host.error(), "first").expect("first line");
        writeln!(host.error(), "second").expect("second line");

        assert_eq!(writeln!(host.output(), "third").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        host.error().flush().expect("flushing a live stream should succeed");
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
        assert_eq!(host.env("PATH"), None);
    }

    /// The sweep helper walks the failure across every line it is told about.
    #[test]
    fn the_sweep_helper_moves_the_failure_along_one_line_at_a_time() {
        fails_at_every_line(3, |host| {
            let mut stream = host.error();

            writeln!(stream, "one")?;
            writeln!(stream, "two")?;
            writeln!(stream, "three")
        });
    }

    /// The work directory lands under the workspace target directory so `cargo clean` sweeps it.
    #[test]
    fn a_work_directory_is_created_under_the_target_directory() {
        let dir = workdir("testing-workdir-");

        assert!(dir.path().is_dir());
        assert!(dir.path().to_string_lossy().contains("test-work"));
    }
}
