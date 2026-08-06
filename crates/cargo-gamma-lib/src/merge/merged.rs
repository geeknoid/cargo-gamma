//! What a merge concluded: the merged report plus the numbers that make it trustworthy.

use std::collections::BTreeSet;

use crate::elements::Report;

/// What a merge concluded.
#[derive(Debug, Default)]
pub struct Merged {
    /// The merged report, ready to render.
    pub report: Option<Report>,

    /// Mutants with a verdict inside the freshness window.
    pub fresh: usize,

    /// Mutants whose most recent verdict predates the window.
    pub stale: usize,

    /// Mutants seen in a report but never actually run.
    pub never_tested: usize,

    /// Distinct shard indices seen.
    pub shards_seen: BTreeSet<u32>,

    /// The shard count the inputs agreed on, when they agreed.
    pub shard_count: Option<u32>,

    /// Inputs whose shard count disagreed with the others.
    ///
    /// Worth reporting rather than resolving: mixing a run at count 30 with one at count 40 means
    /// the two partitioned the population differently, so "shards seen" no longer means coverage.
    pub inconsistent: Vec<String>,

    /// Killed, timed out, or otherwise detected.
    pub detected: usize,

    /// Valid mutants, the denominator of the score.
    pub valid: usize,

    /// Verdicts dropped because the code they were formed against no longer exists.
    ///
    /// A mutant's identity is content-addressed, so editing the code it was generated from gives
    /// the replacement a different id. Without this, the old id stays in the denominator forever:
    /// a survivor that has since been fixed keeps depressing the score, and a caught mutant keeps
    /// crediting code that has changed. Reported rather than silently applied, because a large
    /// number here means the inputs span commits that are further apart than the reader thinks.
    pub withdrawn: usize,
}

impl Merged {
    /// The merged mutation score, as a percentage.
    ///
    /// Returns zero for an empty population rather than a division by zero, and the caller is
    /// expected to report the count beside it so a perfect score over nothing is visibly nothing.
    #[must_use]
    pub fn score(&self) -> f64 {
        if self.valid == 0 {
            return 0.0;
        }

        #[expect(clippy::cast_precision_loss, reason = "mutant counts are far below the f64 integer limit")]
        let ratio = self.detected as f64 / self.valid as f64;

        ratio * 100.0
    }

    /// How much of the rotation the inputs covered, as a percentage.
    #[must_use]
    pub fn coverage(&self) -> f64 {
        let Some(count) = self.shard_count.filter(|count| *count > 0) else {
            return 100.0;
        };

        #[expect(clippy::cast_precision_loss, reason = "shard counts are small")]
        let ratio = self.shards_seen.len() as f64 / f64::from(count);

        ratio * 100.0
    }

    /// The shard indices the rotation has not covered.
    #[must_use]
    pub fn missing_shards(&self) -> Vec<u32> {
        self.shard_count
            .map(|count| (0..count).filter(|index| !self.shards_seen.contains(index)).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HashMap;
    use crate::elements::{FileResult, Framework, Location, MutantResult, Position, RunInfo, ShardInfo, Thresholds};

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
    fn rotation_coverage_reports_the_shards_that_were_missed() {
        let merged = super::super::merge(
            &[
                ("a".to_owned(), report(Some((0, 4)), 100, vec![mutant("aaa", 1, "Killed")])),
                ("b".to_owned(), report(Some((2, 4)), 200, vec![mutant("bbb", 2, "Killed")])),
            ],
            300,
            None,
        );

        assert_eq!(merged.shards_seen.len(), 2);
        assert_eq!(merged.missing_shards(), vec![1, 3]);
        assert!((merged.coverage() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unsharded_reports_merge_and_report_full_coverage() {
        // The parallel-CI case degenerates to this when nobody passed a shard flag.
        let merged = super::super::merge(
            &[("a".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")]))],
            200,
            None,
        );

        assert!(merged.shards_seen.is_empty());
        assert!((merged.coverage() - 100.0).abs() < f64::EPSILON);
        assert!(merged.missing_shards().is_empty());
    }
}
