//! Runtime support injected into crates under mutation test by `cargo-gamma`.
//!
//! `cargo-gamma` rewrites the crate under test so that every mutation site carries a *guard*: a
//! cheap runtime check that activates exactly one mutant. That lets a whole population of mutants
//! live in a single compiled artifact — the *mutant schema*, after Untch, Offutt and Harrold, who
//! introduced the construction in 1993 — instead of requiring one build per mutant. Since a build
//! is by far the most expensive step in the loop, testing a mutant drops from minutes to the cost
//! of launching a process.
//!
//! # What a guard looks like
//!
//! [`a`] is the only function the instrumented source calls, and it appears in one of three shapes,
//! chosen by what Rust will accept in that position:
//!
//! ```text
//! // an expression, whose value the mutant replaces
//! (if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })
//!
//! // a block, whose body the mutant replaces
//! { if ::gamma_rt::a(12u32) { Default::default() } else { ..the real body.. } }
//!
//! // a statement, which the mutant deletes
//! if !::gamma_rt::a(19u32) { self.entries.push(value); }
//! ```
//!
//! Sites nest — in `a + b < c` the `<` site contains the `+` site — and only the `else` arm carries
//! instrumented children. Exactly one mutant is live in a process, so if the `<` mutant is active
//! then no `+` mutant can be, and the taken arm can hold plain original text. That is what keeps
//! the encoding linear in the size of the source rather than exponential in nesting depth.
//!
//! # You do not depend on this crate
//!
//! `cargo-gamma` copies the workspace to a scratch tree, writes this crate into it, and adds the
//! dependency there. Nothing is added to your manifest, nothing is fetched from the network, and
//! your own build is never instrumented. The package is `cargo-gamma-rt` but its library is named
//! `gamma_rt`, which is why instrumented source can say `::gamma_rt::a` without a rename.
//!
//! The copy embedded in the tool is this exact source, so the vendored runtime cannot drift from
//! the one the guards were generated against.
//!
//! # Why this crate has no dependencies
//!
//! It is injected into the dependency graph of the crate under test. A dependency, a feature, or
//! a build script here would perturb feature unification in *the user's* tree, which could change
//! what their code compiles to and therefore what their tests prove. Zero dependencies is a
//! correctness requirement, not a preference.
//!
//! For the same reason [`a`] must stay trivial. It is called at every mutation site of every
//! execution of the suite, so its cost is multiplied by the whole population: a cached atomic load
//! and a comparison, behind a branch the predictor learns immediately.
//!
//! # A worked example
//!
//! Given this function, and a mutant that turns `<` into `<=`:
//!
//! ```rust
//! fn below(a: u32, b: u32) -> bool {
//!     a < b
//! }
//! ```
//!
//! `cargo-gamma` rewrites it in the scratch tree as:
//!
//! ```rust
//! fn below(a: u32, b: u32) -> bool {
//!     if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b }
//! }
//! ```
//!
//! The whole population lives in one binary, and the run launches it once per mutant:
//!
//! ```text
//! GAMMA_ACTIVE=7  ./target/debug/deps/my_crate-abc123   # mutant 7 is live
//! GAMMA_ACTIVE=8  ./target/debug/deps/my_crate-abc123   # mutant 8 is live
//! ./target/debug/deps/my_crate-abc123                   # nothing is live: the baseline
//! ```
//!
//! # Selection protocol
//!
//! The active mutant is named by the [`ACTIVE_VAR`] environment variable, read exactly once per
//! process. The value is a decimal mutant ordinal. [`NONE`] means no mutant is active, which is
//! how the baseline run and every ordinary build behave — including builds of proc macros, where
//! an active mutant could otherwise hang the compiler.
//!
//! An unset, empty, or unparsable value all mean [`NONE`]. There is no error path: a build that
//! links this crate but is not being driven by a mutation run must behave exactly as it did
//! before, and the ordinals are 1-based precisely so that "absent" and "explicitly unmutated" are
//! the same answer.
//!
//! ```rust
//! use gamma_rt::{ACTIVE_VAR, NONE, a, active, any};
//!
//! // In an ordinary process nothing is selected, so every guard takes its original arm.
//! if active() == NONE {
//!     assert!(!any());
//!     assert!(!a(1));
//!     assert!(!a(9_999));
//! }
//!
//! // `a` answers for exactly one ordinal, whichever one this process was launched with.
//! assert_eq!(a(active()), true);
//! assert_eq!(ACTIVE_VAR, "GAMMA_ACTIVE");
//! ```
//!
//! Reading it once, rather than per call, is what makes the guard cheap; it also means a test that
//! sets the variable on itself changes nothing, which is the honest behavior. The run drives
//! selection by launching a fresh process per mutant.
//!
//! # The three entry points
//!
//! [`a`] is what the guards call, and the only one of the three that instrumented source contains.
//! [`active`] and [`any`] are there for the tool's own diagnostics and for anyone inspecting a
//! scratch tree by hand:
//!
//! ```rust
//! use gamma_rt::{active, any};
//!
//! // Useful in a scratch tree when you are trying to work out which mutant a failing
//! // reproduction actually ran.
//! if any() {
//!     println!("mutant {} is live", active());
//! } else {
//!     println!("baseline");
//! }
//! ```

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use core::sync::atomic::{AtomicU32, Ordering};

/// The ordinal reserved for "no mutant is active".
///
/// Mutant ordinals are 1-based so that an unset or unparsable `GAMMA_ACTIVE` is indistinguishable
/// from an explicit request for unmutated behavior.
///
/// ```rust
/// assert_eq!(gamma_rt::NONE, 0);
/// ```
pub const NONE: u32 = 0;

/// The environment variable naming the active mutant.
///
/// Its value is a decimal mutant ordinal. Setting it to [`NONE`], to nothing, or to something that
/// is not a number all select unmutated behavior.
///
/// ```rust
/// assert_eq!(gamma_rt::ACTIVE_VAR, "GAMMA_ACTIVE");
/// ```
pub const ACTIVE_VAR: &str = "GAMMA_ACTIVE";

/// The variable's name as the C interfaces below want it: NUL-terminated, and a compile-time
/// constant so that asking for it costs nothing.
const ACTIVE_VAR_C: &[u8] = b"GAMMA_ACTIVE\0";

/// The cache's "nothing has been read yet" state.
///
/// It has to be a value no real ordinal can take. Ordinals are assigned densely from one, so a
/// population would have to contain `u32::MAX` mutants to reach it; `parse` refuses that value
/// anyway, which closes the gap rather than arguing about how unlikely it is.
const UNREAD: u32 = u32::MAX;

/// The longest value worth reading: ten digits is every `u32`, and the rest is room for spaces.
const READ_LIMIT: usize = 32;

/// Reads the active ordinal out of the environment without allocating.
///
/// The obvious spelling, `std::env::var`, returns a `String` and therefore allocates. This is the
/// one function in the guard's path that touches anything outside the process's own memory, and it
/// runs at whichever guard executes first — possibly inside a block a test is measuring for
/// allocations. So it goes to the platform directly and parses out of borrowed bytes.
///
/// Concurrent first calls may each do this work. That is harmless: they compute the same answer
/// from the same environment, so whichever store lands last is the one every later call would have
/// produced too.
fn read() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `ACTIVE_VAR_C` is NUL-terminated, and `getenv` either returns null or a pointer
        // to a NUL-terminated string owned by the environment. Nothing in this process mutates the
        // environment concurrently: the tool sets the variable before spawning and never again.
        let value = unsafe { getenv(ACTIVE_VAR_C.as_ptr().cast()) };

        if value.is_null() {
            return NONE;
        }

        // SAFETY: `value` is a non-null pointer to a NUL-terminated string, as established above.
        let bytes = unsafe { borrow(value.cast()) };

        parse(bytes)
    }

    #[cfg(windows)]
    {
        let mut buffer = [0_u8; READ_LIMIT];

        // SAFETY: `ACTIVE_VAR_C` is NUL-terminated and `buffer` is writable for the length passed.
        let written = unsafe {
            GetEnvironmentVariableA(
                ACTIVE_VAR_C.as_ptr(),
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            )
        };

        // Zero means unset, and anything at or past the buffer length means the value was too long
        // to be an ordinal in the first place.
        let Ok(length) = usize::try_from(written) else {
            return NONE;
        };

        if length == 0 || length >= buffer.len() {
            return NONE;
        }

        return parse(&buffer[..length]);
    }

    // A platform with neither interface has no way to be told which mutant is live, so it runs the
    // code the author wrote. That is the safe answer: it reports every mutant as surviving rather
    // than silently reporting a mutated program as correct.
    #[cfg(not(any(unix, windows)))]
    NONE
}

/// Borrows a NUL-terminated string as bytes, stopping at [`READ_LIMIT`] so a corrupt environment
/// cannot walk this process off the end of its own memory.
///
/// # Safety
///
/// `start` must point to a NUL-terminated sequence of bytes that stays valid and unmodified for
/// the duration of the call.
#[cfg(unix)]
const unsafe fn borrow<'a>(start: *const u8) -> &'a [u8] {
    let mut length = 0;

    while length < READ_LIMIT {
        // SAFETY: every byte up to `length` is inside the string the caller promised, because the
        // loop stops at the first terminator it sees.
        let at = unsafe { start.add(length) };

        // SAFETY: `at` points at a byte of that same string.
        if unsafe { *at } == 0 {
            break;
        }

        length += 1;
    }

    // SAFETY: `start` is valid for `length` bytes, which the loop above established by reading
    // each of them, and the caller guarantees nothing modifies them while the slice lives.
    unsafe { core::slice::from_raw_parts(start, length) }
}

/// Parses an ordinal out of borrowed bytes, treating anything unexpected as [`NONE`].
///
/// Surrounding ASCII whitespace is tolerated because a value threaded through a shell can pick it
/// up. Everything else — an empty value, a sign, a non-digit, a number too large to be an ordinal
/// — selects unmutated behavior, which is the answer that cannot turn a mutated program into a
/// passing one.
fn parse(bytes: &[u8]) -> u32 {
    let trimmed = bytes.trim_ascii();

    if trimmed.is_empty() {
        return NONE;
    }

    let mut value = 0_u32;

    for byte in trimmed {
        let Some(digit) = (*byte as char).to_digit(10) else {
            return NONE;
        };

        let Some(shifted) = value.checked_mul(10).and_then(|shifted| shifted.checked_add(digit)) else {
            return NONE;
        };

        value = shifted;
    }

    if value == UNREAD { NONE } else { value }
}

#[cfg(unix)]
unsafe extern "C" {
    /// The C library's `getenv`, whose signature is fixed by POSIX. Declared here rather than
    /// taken from `libc` because this crate is injected into the user's dependency graph and must
    /// stay free of dependencies.
    fn getenv(name: *const core::ffi::c_char) -> *const core::ffi::c_char;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    /// `kernel32`'s `GetEnvironmentVariableA`, whose signature is fixed by the Win32 API. Windows
    /// has no POSIX `getenv`, and the copy held by the C library is not guaranteed to see variables
    /// set through the Win32 side of the process.
    fn GetEnvironmentVariableA(name: *const u8, buffer: *mut u8, size: u32) -> u32;
}

/// Returns the ordinal of the mutant active in this process.
///
/// The environment is read without allocating and the answer is cached in an atomic. Reading once
/// is deliberate: a test that manipulates its own environment must not be able to change which
/// mutant is live half way through a run, because that would make the verdict depend on test
/// execution order.
///
/// Not allocating is equally deliberate. This function runs for the first time at whichever guard
/// the process reaches first, which may be inside a region a test is measuring. A library that
/// asserts an operation allocates nothing would see the guard's own allocation and fail, and the
/// failure would look like a broken baseline rather than an artifact of instrumentation. See
/// `read` for how the value is obtained.
///
/// ```rust
/// // Whatever this process was launched with, the answer never changes.
/// assert_eq!(gamma_rt::active(), gamma_rt::active());
/// ```
#[inline]
#[must_use]
pub fn active() -> u32 {
    static ACTIVE: AtomicU32 = AtomicU32::new(UNREAD);

    let cached = ACTIVE.load(Ordering::Relaxed);

    if cached != UNREAD {
        return cached;
    }

    let ordinal = read();

    ACTIVE.store(ordinal, Ordering::Relaxed);
    ordinal
}

/// Returns `true` when the mutant with ordinal `id` is the active one.
///
/// This is the function every injected guard calls. It is a cached atomic load and a comparison,
/// which is what makes the whole schema approach affordable: an inactive guard costs a predictable
/// branch that the CPU learns immediately.
///
/// The name is one character because it appears at every mutation site of every rewritten file,
/// where it is read far less often than it is written.
///
/// ```rust
/// // What an instrumented `a < b` becomes, for the mutant with ordinal 7.
/// let (a_val, b_val) = (1_u32, 2_u32);
/// let result = if gamma_rt::a(7) { a_val <= b_val } else { a_val < b_val };
///
/// assert!(result);
/// ```
#[inline]
#[must_use]
pub fn a(id: u32) -> bool {
    active() == id
}

/// Returns `true` when any mutant is active in this process.
///
/// Nothing in the instrumented source calls this; it is for diagnostics, and for code that wants
/// to know whether it is running under mutation at all.
///
/// ```rust
/// assert_eq!(gamma_rt::any(), gamma_rt::active() != gamma_rt::NONE);
/// ```
#[inline]
#[must_use]
pub fn any() -> bool {
    active() != NONE
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::{GlobalAlloc, Layout};
    use core::sync::atomic::AtomicUsize;
    use std::alloc::System;
    use std::sync::{Mutex, PoisonError};

    /// Counts every allocation the test binary makes, so a test can prove a region made none.
    struct Counting;

    /// How many times [`Counting`] has been asked for memory.
    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    // SAFETY: every method forwards to the system allocator with the arguments it was given, so
    // the contract is exactly the system allocator's, which upholds it.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ALLOCATIONS.fetch_add(1, Ordering::Relaxed);

            // SAFETY: `layout` is whatever the caller passed, which is what `System` expects.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the pointer and layout come from a matching `alloc` on this same allocator,
            // which forwarded to `System`.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = ALLOCATIONS.fetch_add(1, Ordering::Relaxed);

            // SAFETY: as for `dealloc`, plus the size is the caller's, which is what `System` wants.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Runs `body` and reports how many allocations it made.
    /// Counts the allocations `body` makes.
    ///
    /// The counter is global to the process while the test harness runs tests on parallel threads,
    /// so any allocation another test happens to make during `body` would be counted here too.
    /// That is not a theoretical race: it fails in a full workspace run, where this binary competes
    /// with everything else for cores, and passes on its own, which is the worst possible shape for
    /// a test to fail in. Holding a lock for the measurement is what makes the number mean what it
    /// says.
    fn allocations(body: impl FnOnce()) -> usize {
        static MEASURING: Mutex<()> = Mutex::new(());

        // A panic in an earlier measurement poisons the lock, and the count is still valid after
        // one, so the guard is taken either way.
        let _measuring = MEASURING.lock().unwrap_or_else(PoisonError::into_inner);

        let before = ALLOCATIONS.load(Ordering::Relaxed);

        body();

        ALLOCATIONS.load(Ordering::Relaxed) - before
    }

    #[test]
    fn none_is_zero_so_unset_means_baseline() {
        assert_eq!(NONE, 0);
    }

    #[test]
    fn active_is_stable_across_calls() {
        // Whatever the ambient environment is, the answer must not change between calls.
        let first = active();
        let second = active();

        assert_eq!(first, second);
    }

    #[test]
    fn a_matches_only_the_active_ordinal() {
        let live = active();

        assert!(a(live));
        assert!(!a(live.wrapping_add(1)));
    }

    #[test]
    fn any_agrees_with_active() {
        assert_eq!(any(), active() != NONE);
    }

    #[test]
    fn reading_the_environment_allocates_nothing() {
        // This is the whole point of not calling `std::env::var`. The uncached read is measured
        // directly rather than through `active`, because `active` caches and only the first call
        // in the process would ever have done the work.
        assert_eq!(
            allocations(|| {
                let _read = read();
            }),
            0
        );
    }

    #[test]
    fn a_guard_allocates_nothing() {
        // Warm the cache first: what a guard costs on the millionth site is the number that gets
        // multiplied by the whole population.
        let _ = active();

        assert_eq!(
            allocations(|| {
                let _live = a(7);
            }),
            0
        );
    }

    #[test]
    fn an_unset_or_unparsable_value_selects_the_baseline() {
        // Anything that is not a plain decimal number has to mean "run the code the author wrote".
        // Guessing an ordinal instead could report a mutated program as passing.
        assert_eq!(parse(b""), NONE);
        assert_eq!(parse(b"   "), NONE);
        assert_eq!(parse(b"-1"), NONE);
        assert_eq!(parse(b"+1"), NONE);
        assert_eq!(parse(b"7x"), NONE);
        assert_eq!(parse(b"0x7"), NONE);
        assert_eq!(parse("٧".as_bytes()), NONE, "only ASCII digits count");
    }

    #[test]
    fn a_well_formed_value_parses() {
        assert_eq!(parse(b"0"), NONE);
        assert_eq!(parse(b"7"), 7);
        assert_eq!(parse(b" 42 \n"), 42);
        assert_eq!(parse(b"4294967294"), u32::MAX - 1);
    }

    #[test]
    fn a_value_too_large_to_be_an_ordinal_selects_the_baseline() {
        // Overflow must not wrap into a valid ordinal, and the cache's sentinel must not be
        // reachable from the environment or a first read would look like no read at all.
        assert_eq!(parse(b"4294967296"), NONE);
        assert_eq!(parse(b"99999999999999999999"), NONE);
        assert_eq!(parse(b"4294967295"), NONE, "the sentinel is not an ordinal");
    }

    #[test]
    fn an_absurdly_long_value_is_bounded() {
        // A corrupt environment must not be able to walk this process off the end of its memory,
        // and a value this long cannot be an ordinal anyway.
        let long = [b'9'; READ_LIMIT * 4];

        assert_eq!(parse(&long), NONE);
    }

    #[cfg(unix)]
    #[test]
    fn borrowing_stops_at_the_terminator() {
        let text = b"123\x004\x35\x36";

        // SAFETY: `text` is a NUL-terminated byte string that outlives the borrow.
        let bytes = unsafe { borrow(text.as_ptr()) };

        assert_eq!(bytes, b"123");
    }

    #[cfg(unix)]
    #[test]
    fn borrowing_stops_at_the_limit() {
        let text = [b'9'; READ_LIMIT * 2];

        // SAFETY: `text` has no terminator, which is exactly the case the limit exists for, and it
        // is long enough that the limit is reached well inside the allocation.
        let bytes = unsafe { borrow(text.as_ptr()) };

        assert_eq!(bytes.len(), READ_LIMIT);
    }

    #[test]
    fn the_variable_names_agree() {
        // Two spellings of one name is two chances to change only one of them.
        assert_eq!(ACTIVE_VAR.as_bytes(), &ACTIVE_VAR_C[..ACTIVE_VAR_C.len() - 1]);
        assert_eq!(ACTIVE_VAR_C.last(), Some(&0));
    }

    #[test]
    fn the_environment_decides_what_is_read() {
        // Setting a variable in this process would be a data race against every other test, so the
        // check runs in a child launched with the variable already set — which is exactly how the
        // tool passes an ordinal to a real test binary.
        let executable = std::env::current_exe().expect("the test binary knows its own path");

        for (value, expected) in [("31", "read=31"), ("not a number", "read=0"), ("", "read=0")] {
            let output = std::process::Command::new(&executable)
                .args(["--exact", "tests::the_child_reports_what_it_read", "--nocapture"])
                .env(ACTIVE_VAR, value)
                .output()
                .expect("the child runs");

            let text = String::from_utf8_lossy(&output.stdout).into_owned();

            assert!(text.contains(expected), "`{value}` produced {text}");
        }
    }

    #[test]
    fn the_child_reports_what_it_read() {
        // Only meaningful when launched by the test above; harmless on its own.
        println!("read={}", read());
    }
}
