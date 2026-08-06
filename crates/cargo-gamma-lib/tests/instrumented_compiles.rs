//! Every mutant this tool generates has to compile.
//!
//! Parsing is not the same question. The instrumented text is spliced together from source
//! fragments, and a splice can parse perfectly while binding a name that is out of scope, moving a
//! value twice, or leaving a `match` the compiler no longer considers exhaustive. Each of those
//! reaches the user as an unviable mutant: a full build round spent to learn nothing, on a tool
//! whose whole cost is builds.
//!
//! So these tests hand the instrumented text to `rustc` and insist it type-checks. They are slower
//! than the unit tests beside the collector, and they are the only thing that actually answers the
//! question.

use cargo_gamma_lib::ops::collect;
use cargo_gamma_lib::ops::registry::Selection;
use cargo_gamma_lib::parse::SourceFile;
use cargo_gamma_lib::schema;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

/// Builds the stub the guards call, once for the whole test binary.
///
/// The guard path is `::gamma_rt::a`, which names an external crate rather than anything the
/// instrumented file could declare for itself, so there has to be a real crate to point at.
fn guard_crate() -> Option<&'static Path> {
    static BUILT: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let directory = std::env::temp_dir().join("cargo-gamma-instrumented-compiles");

            std::fs::create_dir_all(&directory).ok()?;

            let source = directory.join("gamma_rt.rs");
            let library = directory.join("libgamma_rt.rlib");

            let stub = "#[inline] pub fn a(_ordinal: u32) -> bool { std::hint::black_box(false) }\n";

            // Always false, so the compiler sees both branches as live and type-checks the
            // replacement as well as the original. A `const true` would let it discard one.
            std::fs::write(&source, stub).ok()?;

            let built = Command::new(rustc())
                .args(["--edition", "2024", "--crate-type", "lib", "--crate-name", "gamma_rt", "-o"])
                .arg(&library)
                .arg(&source)
                .output()
                .ok()?;

            built.status.success().then_some(library)
        })
        .as_deref()
}

fn rustc() -> String {
    std::env::var("RUSTC").unwrap_or_else(|_missing| "rustc".to_owned())
}

/// Instruments `source` with every mutant `ops` selects and type-checks the result.
///
/// Returns how many mutants were spliced in, so a test cannot pass by generating nothing at all —
/// which is the failure mode a compile check is least able to notice on its own.
#[track_caller]
fn compiles(name: &str, source: &str, ops: &str) -> usize {
    let Some(guard) = guard_crate() else {
        // A host without a working `rustc` cannot answer the question. Failing here would report a
        // problem with the environment as a problem with the tool.
        eprintln!("skipping {name}: no usable rustc");

        return usize::MAX;
    };

    let file = SourceFile::parse("subject.rs", source.to_owned()).expect("the subject must parse");
    let selection = Selection::parse(ops).expect("the selector must resolve");
    let candidates = collect::collect(&file, &selection);
    let mutants = collect::into_mutants(&file, "subject", candidates);
    let refs: Vec<&_> = mutants.iter().collect();

    let instrumented = schema::instrument(&file.text, &refs).expect("the mutants must splice");
    let directory = std::env::temp_dir().join("cargo-gamma-instrumented-compiles");
    let path = directory.join(format!("{name}.rs"));

    std::fs::write(&path, &instrumented).expect("the instrumented source must be writable");

    let checked = Command::new(rustc())
        .args(["--edition", "2024", "--crate-type", "lib", "--emit", "metadata"])
        .arg("--extern")
        .arg(format!("gamma_rt={}", guard.display()))
        .arg("-o")
        .arg(directory.join(format!("{name}.rmeta")))
        .arg(&path)
        .output()
        .expect("rustc must run");

    assert!(
        checked.status.success(),
        "the instrumented source does not compile\n--- source ---\n{instrumented}\n--- rustc ---\n{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    mutants.len()
}

#[test]
fn a_disabled_match_arm_still_compiles() {
    // The guard is spliced into the pattern position, where the arm's bindings are in scope but
    // not yet moved. Getting this wrong is not a parse error, it is a borrow error.
    let source = "
pub fn classify(value: Option<String>) -> String {
    match value {
        Some(text) if text.is_empty() => String::from(\"empty\"),
        Some(text) => text,
        None => String::from(\"none\"),
        _ => String::from(\"other\"),
    }
}
";

    assert!(compiles("match_arm", source, "match_arm,match_guard") > 0);
}

#[test]
fn an_omitted_struct_field_still_compiles() {
    let source = "
#[derive(Default)]
pub struct Config { pub timeout: u32, pub retries: u32, pub name: String }

pub fn build(timeout: u32) -> Config {
    Config { timeout, retries: 3, ..Default::default() }
}
";

    assert!(compiles("struct_field", source, "struct_field") > 0);
}

#[test]
fn a_moved_range_boundary_still_compiles() {
    let source = "
pub fn total(values: &[u32], n: usize) -> u32 {
    let mut sum = 0;

    for index in 0..n {
        sum += values[index];
    }

    for value in &values[..n] {
        sum += *value;
    }

    for step in 1..=n {
        sum += step as u32;
    }

    sum
}
";

    assert!(compiles("range", source, "range") > 0);
}

#[test]
fn swapped_loop_exits_still_compile() {
    let source = "
pub fn first_even(values: &[i32]) -> i32 {
    let mut found = 0;

    'outer: for value in values {
        for _inner in 0..2 {
            if *value % 2 != 0 {
                continue 'outer;
            }

            if *value > 100 {
                break;
            }

            found = *value;
        }
    }

    found
}
";

    assert!(compiles("loop_exits", source, "loop") > 0);
}

#[test]
fn perturbed_numeric_expressions_still_compile() {
    let source = "
pub fn lookup(values: &[u32], index: usize, count: usize) -> u32 {
    let scaled = scale(count);

    if scaled > 0 {
        return values[index];
    }

    values[index] + scaled
}

fn scale(count: usize) -> u32 {
    count as u32
}
";

    assert!(compiles("perturbation", source, "expr") > 0);
}

#[test]
fn every_new_family_at_once_still_compiles() {
    // The families nest: an arm guard inside a match inside a loop whose bounds are themselves
    // moved. Nesting is where a splice that is individually correct stops being so.
    //
    // The selector names the new families rather than `all`, and that is not the test avoiding an
    // inconvenient answer. Some mutants genuinely cannot compile — negating an unsigned literal is
    // the standard example — and the build converges by blaming the diagnostics, withdrawing those
    // mutants and going round again. Being unviable is a supported outcome, so `all` is not a
    // question with a yes-or-no answer. What is not supported is a family that is unviable
    // *systematically*, because then every run pays a build round to withdraw work it should never
    // have generated, and that is exactly what these tests exist to catch.
    let source = "
#[derive(Default)]
pub struct Limits { pub floor: usize, pub ceiling: usize }

pub fn bound(values: &[usize], mode: usize) -> usize {
    let limits = Limits { floor: 1, ceiling: values.len(), ..Default::default() };
    let mut total = 0;

    for index in limits.floor..=limits.ceiling {
        if index >= values.len() {
            break;
        }

        match mode {
            0 if values[index] > total => total += values[index],
            1 => continue,
            _ => total += index,
        }
    }

    total
}
";

    assert!(compiles("everything", source, "match_arm,match_guard,struct_field,range,loop,expr") > 0);
}

#[test]
fn recursive_typed_return_values_still_compile() {
    // The nested cases are the point: a `Result<Option<bool>, E>` has to compose three levels of
    // replacement and still name a value the compiler accepts at each one.
    let source = "
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::rc::Rc;

pub fn nested() -> Result<Option<bool>, String> { Ok(Some(true)) }
pub fn pair() -> (u32, bool) { (1, true) }
pub fn deque() -> VecDeque<u32> { VecDeque::new() }
pub fn set() -> BTreeSet<u32> { BTreeSet::new() }
pub fn map() -> BTreeMap<String, u32> { BTreeMap::new() }
pub fn boxed() -> Box<u32> { Box::new(1) }
pub fn counted() -> Rc<String> { Rc::new(String::new()) }
pub fn borrowed() -> Cow<'static, str> { Cow::Borrowed(\"x\") }
pub fn nonzero() -> NonZeroUsize { NonZeroUsize::new(4).unwrap() }
pub fn iterator() -> impl Iterator<Item = u32> { std::iter::once(1) }
";

    assert!(compiles("returns", source, "fn_value") > 0);
}

#[test]
fn standard_library_semantics_still_compile() {
    let source = "
pub fn shapes(words: &[String], text: &str, limit: usize) -> usize {
    let mut total = 0;

    if words.iter().any(|word| word.starts_with(text)) {
        total += 1;
    }

    if words.iter().all(|word| word.ends_with(text)) {
        total += 1;
    }

    let taken: Vec<_> = words.iter().take(limit).collect();
    let skipped: Vec<_> = words.iter().skip(limit).collect();
    let filtered: Vec<_> = words.iter().filter(|word| !word.is_empty()).rev().collect();

    total += taken.len() + skipped.len() + filtered.len();
    total += words.first().map_or(0, String::len);
    total += words.last().map_or(0, String::len);
    total += words.iter().map(String::len).min().unwrap_or(0);
    total += words.iter().map(String::len).max().unwrap_or(0);
    total += text.to_lowercase().len() + text.to_uppercase().len();
    total += text.trim_start().len() + text.trim_end().len();

    let mut owned: Vec<usize> = vec![3, 1, 2];

    owned.sort();
    owned.dedup();

    total += owned.len();
    total
}

pub fn optional(flag: bool) -> Option<u32> {
    if flag { Some(1) } else { None }
}

pub fn fallible(flag: bool) -> Result<u32, String> {
    if flag { Ok(1) } else { Err(String::new()) }
}

pub fn assigned(mut value: u32) -> u32 {
    value = value + 1;
    value
}
";

    assert!(compiles("semantics", source, "option,result,iter,string,collection,assign_value") > 0);
}
