use std::fs;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;

use crate::elements::Report;
use crate::error::error;
use crate::report::Styler;

use super::cli::MergeArgs;
use super::dispatch::{EXIT_GATE_FAILED, EXIT_OK};
use super::host::Host;

/// Implements `merge`.
pub(super) fn merge<H: Host>(host: &mut H, args: &MergeArgs, styler: Styler) -> crate::Result<i32> {
    let inputs = collect_reports(&args.inputs)?;

    if inputs.is_empty() {
        return Err(error!("no reports were found in the given paths").usage());
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());

    let window = (args.window > 0).then(|| args.window.saturating_mul(86_400));
    let merged = crate::merge::merge(&inputs, now, window);

    report_merge(host, args, &merged, styler)?;

    if let Some(report) = merged.report.as_ref() {
        if let Some(path) = args.json_report.as_ref() {
            crate::elements::write(path, &crate::elements::to_json(report)?)?;
            writeln!(host.error(), "{} {path}", styler.verb("Wrote"))?;
        }

        if let Some(path) = args.html.as_ref() {
            crate::elements::write(path, &crate::html::render(report, crate::html::Source::Inline)?)?;
            writeln!(host.error(), "{} {path}", styler.verb("Wrote"))?;
        }
    }

    if let Some(minimum) = args.min_score
        && merged.score() < minimum
    {
        writeln!(
            host.error(),
            "{} merged mutation score {:.1}% is below the required {minimum:.1}%",
            styler.error("error:"),
            merged.score()
        )?;

        return Ok(EXIT_GATE_FAILED);
    }

    Ok(EXIT_OK)
}

/// Reads every report named, expanding directories to the JSON files they contain.
///
/// Directories are accepted because the natural place to keep a rotation's history is a directory,
/// and requiring a glob would mean the command behaves differently under shells that do not expand
/// one.
fn collect_reports(inputs: &[Utf8PathBuf]) -> crate::Result<Vec<(String, Report)>> {
    let mut out = Vec::new();

    for input in inputs {
        if input.is_dir() {
            let entries = fs::read_dir(input)
                .map_err(|cause| error!("could not read {input}").caused_by(cause))?;
            let mut paths: Vec<Utf8PathBuf> = Vec::new();

            for entry in entries {
                let entry = entry.map_err(|cause| error!("could not read {input}").caused_by(cause))?;
                let path = Utf8PathBuf::from_path_buf(entry.path())
                    .map_err(|path| error!("{} is not a UTF-8 path", path.display()))?;

                if path.extension() == Some("json") {
                    paths.push(path);
                }
            }

            // Directory order is not defined, and the merge must not depend on it.
            paths.sort();

            for path in paths {
                out.push((path.to_string(), crate::merge::read(&path)?));
            }
        } else {
            out.push((input.to_string(), crate::merge::read(input)?));
        }
    }

    Ok(out)
}

/// Prints what the merge concluded.
fn report_merge<H: Host>(host: &mut H, args: &MergeArgs, merged: &crate::merge::Merged, styler: Styler) -> crate::Result<()> {
    let mut stream = host.error();

    writeln!(
        stream,
        "{} {} caught, {} missed, score {:.1}%",
        styler.verb("Merged"),
        merged.detected,
        merged.valid.saturating_sub(merged.detected),
        merged.score()
    )?;

    // The score alone is not an answer for a rotation: it is the score of whatever happens to have
    // been tested, and these three numbers are what say how much of the codebase that was.
    writeln!(
        stream,
        "{} {} fresh, {} older than {} days, {} never tested",
        styler.verb("Freshness"),
        merged.fresh,
        merged.stale,
        args.window,
        merged.never_tested
    )?;

    if merged.withdrawn > 0 {
        writeln!(
            stream,
            "{} {} dropped, tested against code that has since changed",
            styler.verb("Withdrawn"),
            merged.withdrawn
        )?;
    }

    if let Some(count) = merged.shard_count {
        writeln!(
            stream,
            "{} {} of {count} shards seen, {:.0}% of the rotation",
            styler.verb("Rotation"),
            merged.shards_seen.len(),
            merged.coverage()
        )?;

        let missing = merged.missing_shards();

        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(u32::to_string).collect();

            writeln!(
                stream,
                "{} shards never run: {}",
                styler.verb("Note"),
                names.join(", ")
            )?;
        }
    }

    // Two runs at different shard counts partitioned the population differently, so the coverage
    // number above is not the claim it appears to be.
    for input in &merged.inconsistent {
        writeln!(
            stream,
            "{} {input} used a different shard count; rotation coverage is not meaningful across it",
            styler.verb("Warning")
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::HashMap;
    use crate::elements::{FileResult, Framework, Location, MutantResult, Position, RunInfo, ShardInfo, Thresholds};

    use super::*;
    use crate::testing::{Sink, fails_at_every_line, workdir};

    fn mutant(id: &str, line: usize, status: &str) -> MutantResult {
        MutantResult {
            id: id.to_owned(),
            mutator_name: "relational.lt_to_le".to_owned(),
            location: Location {
                start: Position { line, column: 1 },
                end: Position { line, column: 2 },
            },
            status: status.to_owned(),
            replacement: None,
            description: None,
            status_reason: None,
            duration: None,
            killed_by: None,
        }
    }

    fn report(index: u32, count: u32, status: &str) -> Report {
        let mut files = HashMap::default();
        let _ = files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "pub fn f() {}\n".to_owned(),
                language: "rust".to_owned(),
                mutants: vec![mutant(&format!("m{index}"), usize::try_from(index + 1).unwrap(), status)],
            },
        );

        Report {
            schema_version: "2".to_owned(),
            thresholds: Thresholds::default(),
            project_root: None,
            framework: Framework {
                name: "cargo-gamma".to_owned(),
                version: "0.1.0".to_owned(),
            },
            files,
            config: Some(RunInfo {
                started_at: 100 + u64::from(index),
                shard: Some(ShardInfo { index, count }),
            }),
        }
    }

    fn write_report(path: &camino::Utf8Path, report: &Report) {
        crate::elements::write(&path.to_path_buf(), &crate::elements::to_json(report).expect("json")).expect("write");
    }

    #[test]
    fn directories_are_scanned_and_outputs_are_written() {
        let dir = workdir("merge-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");
        write_report(&input_dir.join("a.json"), &report(0, 3, "Killed"));
        write_report(&input_dir.join("b.json"), &report(1, 4, "Survived"));
        fs::write(input_dir.join("ignored.txt"), "not a report").expect("ignore");

        let args = MergeArgs {
            inputs: vec![input_dir],
            json_report: Some(root.join("out/report.json")),
            html: Some(root.join("out/report.html")),
            window: 30,
            min_score: Some(75.0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(root.join("out/report.json").exists());
        assert!(root.join("out/report.html").exists());
        assert!(err.contains("shards never run"), "{err}");
        assert!(err.contains("different shard count"), "{err}");
        assert!(err.contains("below the required"), "{err}");
        assert!(host.out.is_empty());
    }

    #[test]
    fn empty_inputs_are_a_usage_error() {
        let dir = workdir("merge-empty-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = MergeArgs {
            inputs: vec![root],
            json_report: None,
            html: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let err = merge(&mut host, &args, Styler::new(false)).unwrap_err();

        assert!(err.is_usage());
    }

    /// A closed diagnostic stream has to surface from every line the merge prints.
    #[test]
    fn a_closed_diagnostic_stream_is_reported_from_any_line() {
        let dir = workdir("merge-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");
        write_report(&input_dir.join("a.json"), &report(0, 3, "Killed"));
        write_report(&input_dir.join("b.json"), &report(1, 4, "Survived"));

        let args = MergeArgs {
            inputs: vec![input_dir],
            json_report: Some(root.join("out/report.json")),
            html: Some(root.join("out/report.html")),
            window: 30,
            min_score: Some(75.0),
        };

        fails_at_every_line(8, |host| merge(host, &args, Styler::new(false)).map(|_| ()));
    }

    /// Merging without a gate or any report to write still succeeds and says what it saw.
    #[test]
    fn a_merge_with_no_gate_and_no_outputs_succeeds() {
        let dir = workdir("merge-plain-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &report(0, 1, "Killed"));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html: None,
            window: 30,
            min_score: Some(10.0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("Merged"), "{}", host.err());
        assert!(!host.err().contains("shards never run"), "{}", host.err());
    }

    /// A directory that cannot be read names itself rather than failing anonymously.
    #[test]
    fn an_unreadable_input_directory_names_itself() {
        let dir = workdir("merge-unreadable-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let missing = root.join("gone");

        let args = MergeArgs {
            inputs: vec![missing.clone()],
            json_report: None,
            html: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let error = merge(&mut host, &args, Styler::new(false)).expect_err("missing input");

        assert!(error.to_string().contains(missing.as_str()), "{error}");
    }
}
