use std::env;
use std::io::Write;

/// Everything the library needs from the outside world.
///
/// Routing all output and all terminal interrogation through one trait is what makes the console
/// UI testable. A fake host captures both streams and reports a fixed width, so the progress
/// rendering, the color decisions and the exit codes are all ordinary assertions in an
/// integration test rather than things verified by eye.
pub trait Host {
    /// The stream for results the user might pipe into another program.
    fn output(&mut self) -> impl Write;

    /// The stream for progress and diagnostics.
    fn error(&mut self) -> impl Write;

    /// Whether the diagnostic stream is a terminal.
    fn is_terminal(&self) -> bool;

    /// The width of the terminal in columns, if there is one.
    fn terminal_width(&self) -> Option<u16>;

    /// The value of an environment variable.
    ///
    /// Reading the real environment is right for every caller but a test, and a test that wants to
    /// pretend it is running inside a CI runner should not have to mutate the process it shares
    /// with every other test to do it.
    fn env(&self, name: &str) -> Option<String> {
        env::var(name).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PlainHost;

    impl Host for PlainHost {
        fn output(&mut self) -> impl Write {
            Vec::new()
        }

        fn error(&mut self) -> impl Write {
            Vec::new()
        }

        fn is_terminal(&self) -> bool {
            false
        }

        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }

    /// A host that overrides nothing gets the real environment, which is right for the real binary.
    #[test]
    fn default_env_reads_the_process_environment() {
        let host = PlainHost;
        let path = host.env("PATH").expect("PATH should be set for cargo test");

        assert!(!path.is_empty());
        assert_eq!(host.env("GAMMA_DEFINITELY_NOT_SET_IN_THE_ENVIRONMENT"), None);
    }

    /// The rest of the contract is answered too, so the default double stays honest.
    #[test]
    fn a_minimal_host_still_answers_the_whole_contract() {
        let mut host = PlainHost;

        host.output().write_all(b"out").expect("output is a sink");
        host.error().write_all(b"err").expect("error is a sink");

        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
    }
}
