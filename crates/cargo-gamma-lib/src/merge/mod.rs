//! Combining per-shard reports into one answer.
//!
//! A single shard cannot answer "what is our mutation score?", and scoring a shard on its own is
//! actively misleading: on a three-hundred-mutant shard one survivor moves the score by a third of a
//! point, so a threshold set on a shard fires on noise. The merged view across a rotation is the real
//! deliverable, and the per-shard report is an intermediate.
//!
//! Merging is a union by stable mutant ID, most recent verdict winning. That works only because IDs
//! are content-addressed: a mutant keeps its ID as the code around it changes, so last night's
//! verdict still refers to the same construct, and a construct that *was* edited gets a different ID
//! and correctly shows up as never tested rather than inheriting a verdict it never earned.
//!
//! A union alone is not enough for the second half of that promise. The edited construct's *new* ID
//! does appear as never tested, but nothing removes the *old* one, so a survivor that has since been
//! fixed goes on depressing the score and a caught verdict goes on crediting code that has changed.
//! Whichever unsharded input is newest states the complete population of every file it contains, so
//! an ID absent from it has been withdrawn, and [`Merged::withdrawn`] counts what that dropped. A
//! sharded input describes only its own slice of the population and can never withdraw anything.
//!
//! Three numbers matter as much as the score, and all three are invisible without merging:
//!
//! - **Never tested.** Code added since the rotation last touched its shard. Reported separately and
//!   never counted as killed, because counting untested code as passing is how a mutation score
//!   becomes a decoration.
//! - **Stale.** A verdict older than the freshness window. Still reported, but not claimed as
//!   current.
//! - **Rotation health.** Shards seen against shards expected. A rotation that is not keeping up is
//!   the actual problem behind a score that will not move.

mod merged;
mod verdict;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use camino::Utf8Path;

use crate::Result;
use crate::elements::{FileResult, MutantResult, Report};
use crate::error::error;

pub use merged::Merged;
use verdict::Verdict;

/// A verdict that means the mutant was never actually run.
const NEVER_RUN: &str = "Pending";

/// Merges reports, keeping the most recent verdict per mutant ID.
///
/// `now` and `window` are passed in rather than read from the clock so that the freshness rule is
/// testable and so that re-merging the same inputs twice gives the same answer.
#[must_use]
pub fn merge(reports: &[(String, Report)], now: u64, window: Option<u64>) -> Merged {
    let mut latest: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut out = Merged::default();
    let current = populations(reports);

    for (name, report) in reports {
        let tested_at = report.config.as_ref().map_or(0, |config| config.started_at);

        if let Some(shard) = report.config.as_ref().and_then(|config| config.shard) {
            let _ = out.shards_seen.insert(shard.index);

            match out.shard_count {
                None => out.shard_count = Some(shard.count),
                Some(count) if count != shard.count => out.inconsistent.push(name.clone()),
                Some(_) => {}
            }
        }

        for (path, file) in &report.files {
            for mutant in &file.mutants {
                // A file whose current population is known admits exactly the ids in it. One that
                // is not known — every input for it was a shard — admits everything, because a
                // shard's absence of an id says nothing about whether the code still exists.
                if current.get(path).is_some_and(|ids| !ids.contains(&mutant.id)) {
                    out.withdrawn += 1;
                    continue;
                }

                let entry = latest.entry(mutant.id.clone());

                match entry {
                    Entry::Vacant(slot) => {
                        let _ = slot.insert(Verdict {
                            mutant: mutant.clone(),
                            file: path.clone(),
                            tested_at,
                        });
                    }

                    // Strictly newer, so a re-run at the same timestamp keeps the first verdict
                    // rather than depending on the order the files were listed on the command line.
                    //
                    // A newer report that never ran the mutant does not overwrite one that did.
                    // The population that says what still exists is usually a listing rather than
                    // a run, and letting it blank out every verdict it is merged with would make
                    // the merged score a report about nothing.
                    Entry::Occupied(mut slot) => {
                        let uninformative = mutant.status == NEVER_RUN && slot.get().mutant.status != NEVER_RUN;

                        if tested_at > slot.get().tested_at && !uninformative {
                            let _ = slot.insert(Verdict {
                                mutant: mutant.clone(),
                                file: path.clone(),
                                tested_at,
                            });
                        }
                    }
                }
            }
        }
    }

    let mut files: BTreeMap<String, Vec<MutantResult>> = BTreeMap::new();

    for verdict in latest.values() {
        if verdict.mutant.status == NEVER_RUN {
            out.never_tested += 1;
        } else if window.is_some_and(|window| now.saturating_sub(verdict.tested_at) > window) {
            out.stale += 1;
        } else {
            out.fresh += 1;
        }

        if is_valid(&verdict.mutant.status) {
            out.valid += 1;

            if is_detected(&verdict.mutant.status) {
                out.detected += 1;
            }
        }

        files.entry(verdict.file.clone()).or_default().push(verdict.mutant.clone());
    }

    out.report = rebuild(reports, files);
    out
}

/// The complete set of mutant ids each file currently admits, where an input says so.
///
/// Only an unsharded report can answer this: it lists every mutant of every file it covers, so an
/// id it does not mention is an id the code no longer produces. A sharded report lists one slice of
/// the population, and reading its silence as a withdrawal would erase most of the rotation.
///
/// Ties go to neither: two unsharded reports with the same timestamp describe the same commit, so
/// the first is as good as the second and taking the first keeps the answer independent of the
/// order the files were named on the command line.
fn populations(reports: &[(String, Report)]) -> BTreeMap<String, BTreeSet<String>> {
    let mut newest: BTreeMap<String, (u64, BTreeSet<String>)> = BTreeMap::new();

    for (_, report) in reports {
        if report.config.as_ref().and_then(|config| config.shard).is_some() {
            continue;
        }

        let at = started_at(report);

        for (path, file) in &report.files {
            let ids: BTreeSet<String> = file.mutants.iter().map(|mutant| mutant.id.clone()).collect();

            match newest.entry(path.clone()) {
                Entry::Vacant(slot) => {
                    let _ = slot.insert((at, ids));
                }

                Entry::Occupied(mut slot) => {
                    if at > slot.get().0 {
                        let _ = slot.insert((at, ids));
                    }
                }
            }
        }
    }

    newest.into_iter().map(|(path, (_, ids))| (path, ids)).collect()
}

/// Whether a status counts toward the denominator.
///
/// A mutant that could not compile never tested anything, and one that was suppressed was excluded
/// on purpose. Counting either would let the score be moved by things that are not the test suite.
fn is_valid(status: &str) -> bool {
    !matches!(status, "CompileError" | "Ignored" | NEVER_RUN)
}

/// Whether a status counts as the suite having noticed.
///
/// A timeout counts, matching the report schema's own metric, so the viewer's number and ours agree
/// rather than differing by a rule nobody can see.
fn is_detected(status: &str) -> bool {
    matches!(status, "Killed" | "Timeout")
}

/// When a report's run started, or zero when it does not say.
fn started_at(report: &Report) -> u64 {
    report.config.as_ref().map_or(0, |config| config.started_at)
}

/// Rebuilds a report document from the merged verdicts.
///
/// Source text is taken from whichever input had it. A file's source can differ between reports from
/// different commits; the newest is not necessarily the one that matches every verdict, and there is
/// no honest way to reconcile that, so the most recent report that contains the file wins and the
/// freshness accounting is what tells the reader how much to trust it.
fn rebuild(reports: &[(String, Report)], files: BTreeMap<String, Vec<MutantResult>>) -> Option<Report> {
    let mut newest: Option<&Report> = None;

    for (_, report) in reports {
        if newest.is_none_or(|current| started_at(report) >= started_at(current)) {
            newest = Some(report);
        }
    }

    let base = newest?;
    let mut merged = base.clone();

    merged.files = files
        .into_iter()
        .filter_map(|(path, mut mutants)| {
            // The source text has to come from the same commit the verdicts were formed against,
            // and only the timestamp knows which report that is. Argument order does not: a shell
            // glob orders by name, so letting it decide would render new mutants over old text.
            let source = reports
                .iter()
                .filter_map(|(_, report)| report.files.get(&path).map(|file| (started_at(report), &file.source)))
                .max_by_key(|(at, _)| *at)
                .map(|(_, source)| source.clone())?;

            mutants.sort_by(|left, right| {
                left.location
                    .start
                    .line
                    .cmp(&right.location.start.line)
                    .then(left.id.cmp(&right.id))
            });

            Some((
                path,
                FileResult {
                    source,
                    language: "rust".to_owned(),
                    mutants,
                },
            ))
        })
        .collect();

    Some(merged)
}

/// Reads a report from a file.
pub fn read(path: &Utf8Path) -> Result<Report> {
    let text = fs::read_to_string(path).map_err(|cause| error!("could not read {path}").caused_by(cause))?;

    serde_json::from_str(&text).map_err(|cause| error!("{path} is not a mutation report").caused_by(cause).usage())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HashMap;
    use crate::elements::{Framework, Location, Position, RunInfo, ShardInfo, Thresholds};

    /// One day, in seconds.
    const DAY: u64 = 86_400;

    fn mutant(id: &str, line: usize, status: &str) -> MutantResult {
        MutantResult {
            id: id.to_owned(),
            mutator_name: "relational.lt_to_le".to_owned(),
            location: Location {
                start: Position { line, column: 1 },
                end: Position { line, column: 9 },
            },
            status: status.to_owned(),
            replacement: None,
            description: None,
            status_reason: None,
            duration: None,
            killed_by: None,
        }
    }

    fn report(shard: Option<(u32, u32)>, started_at: u64, mutants: Vec<MutantResult>) -> Report {
        let mut files = HashMap::default();

        let _ = files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "fn f() {}\n".to_owned(),
                language: "rust".to_owned(),
                mutants,
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
                started_at,
                shard: shard.map(|(index, count)| ShardInfo { index, count }),
            }),
        }
    }

    #[test]
    fn shards_union_into_one_population() {
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.valid, 2);
        assert_eq!(merged.detected, 1);
        assert!((merged.score() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_most_recent_verdict_wins() {
        // The whole point of merging a rotation: last night's answer for a mutant is superseded by
        // tonight's, not averaged with it.
        let merged = merge(
            &[
                ("old".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Survived")])),
                ("new".to_owned(), report(Some((0, 2)), 200, vec![mutant("aaa", 1, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.valid, 1);
        assert_eq!(merged.detected, 1);
    }

    #[test]
    fn an_older_report_never_overwrites_a_newer_one() {
        // Argument order is whatever the shell's glob produced, so it must not decide the answer.
        let merged = merge(
            &[
                ("new".to_owned(), report(Some((0, 2)), 200, vec![mutant("aaa", 1, "Killed")])),
                ("old".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.detected, 1);
    }

    #[test]
    fn the_source_text_comes_from_the_newest_report_not_the_last_argument() {
        // A glob orders by filename, which has nothing to do with when a run happened. Taking the
        // last one would render fresh verdicts over source from an older commit.
        let mut newer = report(Some((0, 2)), 200, vec![mutant("aaa", 1, "Killed")]);

        if let Some(file) = newer.files.get_mut("src/lib.rs") {
            file.source = "fn f() { todo!() }\n".to_owned();
        }

        let merged = merge(
            &[
                ("a-new".to_owned(), newer),
                ("z-old".to_owned(), report(Some((1, 2)), 100, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        );

        let document = merged.report.expect("a merge of two reports produces a document");
        let file = document.files.get("src/lib.rs").expect("the merged file survives");

        assert_eq!(file.source, "fn f() { todo!() }\n");
    }

    #[test]
    fn a_verdict_older_than_the_window_is_stale_but_still_counted() {
        // Dropping it would silently shrink the denominator, which raises the score by forgetting.
        let merged = merge(
            &[("a".to_owned(), report(Some((0, 2)), 0, vec![mutant("aaa", 1, "Survived")]))],
            40 * DAY,
            Some(30 * DAY),
        );

        assert_eq!(merged.stale, 1);
        assert_eq!(merged.fresh, 0);
        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_never_run_mutant_is_counted_separately_and_never_as_killed() {
        // Counting untested code as passing is how a mutation score becomes a decoration.
        let merged = merge(
            &[(
                "a".to_owned(),
                report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed"), mutant("bbb", 2, "Pending")]),
            )],
            200,
            None,
        );

        assert_eq!(merged.never_tested, 1);
        assert_eq!(merged.valid, 1, "a never-run mutant must stay out of the denominator");
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unviable_and_suppressed_mutants_stay_out_of_the_denominator() {
        let merged = merge(
            &[(
                "a".to_owned(),
                report(
                    Some((0, 2)),
                    100,
                    vec![
                        mutant("aaa", 1, "Killed"),
                        mutant("bbb", 2, "CompileError"),
                        mutant("ccc", 3, "Ignored"),
                    ],
                ),
            )],
            200,
            None,
        );

        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_timeout_counts_as_detected() {
        let merged = merge(
            &[("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Timeout")]))],
            200,
            None,
        );

        assert_eq!(merged.detected, 1);
    }

    #[test]
    fn a_disagreeing_shard_count_is_reported_rather_than_reconciled() {
        // Two runs at different counts partitioned the population differently, so "shards seen" no
        // longer means what it says, and silently picking one would make the coverage number a lie.
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 4)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((1, 8)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.inconsistent, vec!["b".to_owned()]);
    }

    #[test]
    fn the_merged_document_holds_every_mutant_in_line_order() {
        let merged = merge(
            &[
                ("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("bbb", 9, "Killed")])),
                ("b".to_owned(), report(Some((1, 2)), 200, vec![mutant("aaa", 2, "Survived")])),
            ],
            300,
            None,
        );

        let report = merged.report.expect("a merge of two reports produces one");
        let mutants = &report.files["src/lib.rs"].mutants;

        assert_eq!(mutants.len(), 2);
        assert_eq!(mutants[0].location.start.line, 2);
        assert_eq!(mutants[1].location.start.line, 9);
    }

    #[test]
    fn merging_nothing_produces_nothing_rather_than_a_perfect_score() {
        let merged = merge(&[], 0, None);

        assert!(merged.report.is_none());
        assert_eq!(merged.valid, 0);
        assert!((merged.score() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_merged_document_round_trips_through_json() {
        // `merge` reads what `run` wrote, so the two halves have to agree about the format. This is
        // the only test that exercises both directions.
        let merged = merge(
            &[("a".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")]))],
            200,
            None,
        );

        let text = crate::elements::to_json(&merged.report.expect("a report")).expect("serializes");
        let parsed: Report = serde_json::from_str(&text).expect("the document must read back");

        assert_eq!(parsed.files["src/lib.rs"].mutants[0].id, "aaa");
        assert_eq!(parsed.config.expect("run info survives").started_at, 100);
    }

    #[test]
    fn a_verdict_for_edited_code_leaves_the_denominator() {
        // The old survivor's code was edited, so the newer full run does not produce its id at all.
        // Keeping it would go on depressing the score for a construct that no longer exists, which
        // is exactly what the README promises does not happen.
        let merged = merge(
            &[
                ("old".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Survived"), mutant("keep", 2, "Killed")])),
                ("new".to_owned(), report(None, 200, vec![mutant("bbb", 1, "Pending"), mutant("keep", 2, "Pending")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 1, "the edited mutant was not withdrawn");
        assert_eq!(merged.valid, 1, "only the surviving construct counts");
        assert_eq!(merged.detected, 1);
        assert_eq!(merged.never_tested, 1, "the replacement construct has never been run");
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_withdrawn_mutant_leaves_the_rendered_document_too() {
        // A report that still lists the old mutant would render it over source it does not appear
        // in, which is worse than not mentioning it.
        let merged = merge(
            &[
                ("old".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Survived")])),
                ("new".to_owned(), report(None, 200, vec![mutant("bbb", 1, "Pending")])),
            ],
            300,
            None,
        );

        let ids: Vec<&str> = merged
            .report
            .as_ref()
            .expect("a report")
            .files["src/lib.rs"]
            .mutants
            .iter()
            .map(|found| found.id.as_str())
            .collect();

        assert_eq!(ids, vec!["bbb"]);
    }

    #[test]
    fn a_shard_never_withdraws_anything() {
        // A shard lists its own slice of the population, so an id it does not mention may simply
        // belong to another shard. Reading that silence as a withdrawal would erase the rotation.
        let merged = merge(
            &[
                ("one".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("two".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 0);
        assert_eq!(merged.valid, 2);
    }

    #[test]
    fn a_full_run_withdraws_from_a_shard_that_predates_it() {
        // This is the rotation's real shape: nightly shards, and one full population to say what
        // still exists.
        let merged = merge(
            &[
                ("night".to_owned(), report(Some((0, 4)), 100, vec![mutant("gone", 1, "Survived")])),
                ("today".to_owned(), report(None, 200, vec![mutant("here", 1, "Pending")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 1);
        assert_eq!(merged.valid, 0, "the only construct left has never been tested");
    }

    #[test]
    fn a_newer_shard_is_not_withdrawn_by_an_older_full_run() {
        // The full population is a statement about its own commit. A shard run afterwards may have
        // found a construct that did not exist then, and dropping it would lose a real verdict.
        let merged = merge(
            &[
                ("full".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Pending")])),
                ("later".to_owned(), report(Some((0, 4)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 1, "the older full run still speaks for its own commit");
    }

    #[test]
    fn the_newest_full_population_is_the_one_that_decides() {
        // Two full runs disagree about what exists; the later one is the current tree.
        let merged = merge(
            &[
                ("older".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")])),
                ("newest".to_owned(), report(None, 300, vec![mutant("aaa", 1, "Pending"), mutant("bbb", 2, "Pending")])),
                ("middle".to_owned(), report(None, 200, vec![mutant("ccc", 3, "Survived")])),
            ],
            400,
            None,
        );

        assert_eq!(merged.withdrawn, 1, "only `ccc` is gone");
        assert_eq!(merged.detected, 1, "`aaa` keeps the verdict it earned");
    }

    #[test]
    fn a_listing_does_not_erase_the_verdicts_it_is_merged_with() {
        // The current population usually comes from a listing, which reports every mutant as never
        // run. Newest-wins on its own would let that blank every verdict in the merge and report a
        // score of zero over a suite that had actually killed everything.
        let merged = merge(
            &[
                ("run".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")])),
                ("listing".to_owned(), report(None, 200, vec![mutant("aaa", 1, "Pending")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.detected, 1, "the verdict survived the listing");
        assert_eq!(merged.never_tested, 0);
        assert!((merged.score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_real_verdict_still_replaces_an_older_one() {
        // The rule above must not go so far that a genuine re-run cannot change a verdict.
        let merged = merge(
            &[
                ("old".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")])),
                ("new".to_owned(), report(None, 200, vec![mutant("aaa", 1, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.detected, 0, "the newer run said it survived");
        assert_eq!(merged.valid, 1);
    }

    #[test]
    fn a_file_no_full_run_covers_is_left_alone() {
        // Nothing here states a population for the file, so nothing may claim an id is withdrawn.
        let merged = merge(
            &[
                ("one".to_owned(), report(Some((0, 2)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("two".to_owned(), report(Some((1, 2)), 200, vec![mutant("bbb", 2, "Survived")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.withdrawn, 0);
        assert_eq!(merged.valid, 2);
    }
}
