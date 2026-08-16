use core::fmt;
#[cfg(unix)]
use core::panic::AssertUnwindSafe;
#[cfg(unix)]
use core::time::Duration;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
#[cfg(unix)]
use std::sync::mpsc;

use camino::{Utf8Path, Utf8PathBuf};

const HELPER_SOURCE: &str = r#"
thread_local! {
    static HELD: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn main() {
    let mut code = 0;

    for argument in std::env::args().skip(1) {
        let Some(directive) = argument.strip_prefix("--gamma-step=") else {
            continue;
        };

        if let Some(status) = step(directive) {
            code = status;
            break;
        }
    }

    std::process::exit(code);
}

fn step(directive: &str) -> Option<i32> {
    let (name, payload) = directive.split_once(':').unwrap_or((directive, ""));

    match name {
        "sleep" => {
            let ms = payload.parse().unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            None
        }
        "exit" => Some(payload.parse().unwrap_or(0)),
        "touch" => {
            let _ = std::fs::write(payload, b"");
            None
        }
        "spawn" => {
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("the helper knows its own path"));

            for inner in payload.split('|') {
                let _ = child.arg(format!("--gamma-step={inner}"));
            }

            let _spawned = child.spawn().expect("the helper can start another copy of itself");
            None
        }
        "eat" => {
            let mib: usize = payload.parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();

            for _ in 0..mib {
                let mut block = vec![0_u8; 1024 * 1024];

                for at in (0..block.len()).step_by(4096) {
                    block[at] = 1;
                }

                held.push(block);
            }

            HELD.with(|slot| slot.borrow_mut().extend(held));
            None
        }
        _ => Some(97),
    }
}
"#;

pub fn helper_binary_path() -> &'static Utf8Path {
    static BUILT: OnceLock<Utf8PathBuf> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let work = Utf8PathBuf::from_path_buf(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work"))
                .expect("the target path is UTF-8");

            fs::create_dir_all(work.as_std_path()).expect("the test work directory should be creatable");

            let suffix = if cfg!(windows) { ".exe" } else { "" };
            let helper = work.join(format!("gamma-process-helper-1{suffix}"));

            if helper.exists() {
                return helper;
            }

            let staging = tempfile::Builder::new()
                .prefix("gamma-process-helper-build")
                .tempdir_in(work.as_std_path())
                .expect("the staging directory should be creatable");
            let source = staging.path().join("helper.rs");
            let staged = staging.path().join(format!("helper{suffix}"));

            fs::write(&source, HELPER_SOURCE).expect("the helper source should be writable");

            let built = Command::new("rustc")
                .arg("--edition")
                .arg("2024")
                .arg("-C")
                .arg("debuginfo=0")
                .arg("-o")
                .arg(&staged)
                .arg(&source)
                .output()
                .expect("rustc should be available to the test suite");

            assert!(
                built.status.success(),
                "the test helper should compile: {}",
                String::from_utf8_lossy(&built.stderr)
            );

            let _moved = fs::rename(&staged, helper.as_std_path());

            assert!(helper.exists(), "the test helper should be in place at {helper}");

            helper
        })
        .as_path()
}

pub fn directive(step: impl fmt::Display) -> String {
    format!("--gamma-step={step}")
}

pub fn workdir(prefix: &str) -> tempfile::TempDir {
    let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work");

    fs::create_dir_all(&work).expect("the test work directory should be creatable");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(work)
        .expect("the temporary directory should be creatable")
}

pub fn without_memory_support(what: &str) -> bool {
    match crate::support() {
        Ok(()) => false,
        Err(reason) => {
            eprintln!("standing down: {what} - {reason}");
            true
        }
    }
}

#[cfg(unix)]
pub const WATCHDOG: Duration = Duration::from_secs(60);

#[cfg(unix)]
pub fn within<T: Send + 'static>(budget: Duration, what: &str, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();

    let _worker = std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(body));
        let _delivered = sender.send(outcome);
    });

    match receiver.recv_timeout(budget) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("{what} did not finish within {budget:?}; it is hung"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} ended without an answer"),
    }
}
