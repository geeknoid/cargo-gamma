use serde::{Deserialize, Serialize};

use crate::HashMap;

use super::mutant::Mutant;
use super::outcome::Outcome;

/// Aggregate counts and scores for a set of mutants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub killed: u32,
    pub survived: u32,
    pub timeout: u32,
    pub out_of_memory: u32,
    pub unviable: u32,
    pub ignored: u32,
    pub uncovered: u32,
    pub not_built: u32,
    pub pending: u32,
}

impl Summary {
    /// Tallies a set of mutants.
    #[must_use]
    pub fn of(mutants: &[Mutant]) -> Self {
        let mut summary = Self::default();

        for mutant in mutants {
            let counter = match mutant.outcome {
                Outcome::Killed => &mut summary.killed,
                Outcome::Survived => &mut summary.survived,
                Outcome::Timeout => &mut summary.timeout,
                Outcome::OutOfMemory => &mut summary.out_of_memory,
                Outcome::CompileError => &mut summary.unviable,
                Outcome::Ignored => &mut summary.ignored,
                Outcome::NoCoverage => &mut summary.uncovered,
                Outcome::NotBuilt => &mut summary.not_built,
                Outcome::Pending => &mut summary.pending,
            };

            *counter += 1;
        }

        summary
    }

    /// Number of mutants that count toward the score.
    #[must_use]
    pub const fn valid(self) -> u32 {
self.killed + self.survived + self.timeout + self.out_of_memory + self.uncovered
    }

    /// Number of mutants the suite detected.
    #[must_use]
    pub const fn detected(self) -> u32 {
        self.killed + self.timeout + self.out_of_memory
    }

    /// The mutation score: detected over valid, as a percentage.
    ///
    /// Uncovered mutants are in the denominator and never in the numerator, so they count against
    /// the score exactly as survivors do. That is deliberate, and not in tension with reporting
    /// them separately: the two facts answer different questions. *Which* mutants went undetected,
    /// and why, is a diagnosis — and "no test links this code" is a different problem from "the
    /// tests that run it did not notice", so the two are never merged in a report. *How much* of
    /// the code is defended is a single number, and code no test reaches is undefended.
    ///
    /// This is also the shared schema's own definition, so this number and the one the standard
    /// report UI computes agree exactly. See [`Self::covered_score`] for the other view.
    #[must_use]
    pub fn score(self) -> f64 {
        let valid = self.valid();

        if valid == 0 {
            return 100.0;
        }

        f64::from(self.detected()) * 100.0 / f64::from(valid)
    }

    /// The covered score: detected over the mutants some test actually reaches.
    ///
    /// This measures the quality of the tests that exist, rather than mixing in the question of
    /// what is tested at all.
    #[must_use]
    pub fn covered_score(self) -> f64 {
        let covered = self.detected() + self.survived;

        if covered == 0 {
            return 100.0;
        }

        f64::from(self.detected()) * 100.0 / f64::from(covered)
    }
}

/// Counts of mutants per mutator name, for the operator yield report.
#[must_use]
pub fn yield_by_mutator(mutants: &[Mutant]) -> Vec<(String, Summary)> {
    let mut buckets: HashMap<String, Vec<Mutant>> = HashMap::default();

    for mutant in mutants {
        buckets.entry(mutant.mutator.clone()).or_default().push(mutant.clone());
    }

    let mut rows: Vec<(String, Summary)> = buckets
        .into_iter()
        .map(|(name, group)| {
            let summary = Summary::of(&group);

            (name, summary)
        })
        .collect();

    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::ops::collect::Shape;

    fn mutant(mutator: &str, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("{mutator}:{outcome:?}"),
            ordinal: 1,
            file: Utf8PathBuf::from("src/lib.rs"),
            package: "subject".to_owned(),
            span: 0..1,
            line: 1,
            column: 1,
            mutator: mutator.to_owned(),
            item_path: "subject::f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a + b".to_owned(),
            replacement: "a - b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    #[test]
    fn score_uses_detected_over_valid() {
        let summary = Summary {
            killed: 7,
            survived: 2,
            timeout: 1,
            out_of_memory: 0,
            unviable: 5,
            ignored: 3,
            uncovered: 0,
            not_built: 0,
            pending: 0,
        };

        assert_eq!(summary.valid(), 10);
        assert_eq!(summary.detected(), 8);
        assert!((summary.score() - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn covered_score_excludes_uncovered_mutants() {
        let summary = Summary {
            killed: 4,
            survived: 4,
            timeout: 0,
            out_of_memory: 0,
            unviable: 0,
            ignored: 0,
            uncovered: 92,
            not_built: 0,
            pending: 0,
        };

        assert!((summary.score() - 4.0).abs() < f64::EPSILON);
        assert!((summary.covered_score() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_empty_run_scores_one_hundred() {
        let summary = Summary::default();

        assert!((summary.score() - 100.0).abs() < f64::EPSILON);
        assert!((summary.covered_score() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mutator_yield_groups_and_sorts_by_mutator_name() {
        let mutants = vec![
            mutant("zeta.delete", Outcome::Killed),
            mutant("alpha.flip", Outcome::Survived),
            mutant("alpha.flip", Outcome::Timeout),
        ];

        let rows = yield_by_mutator(&mutants);

        // The operator report is deterministic and carries the same score counts as the summary.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "alpha.flip");
        assert_eq!(rows[0].1.survived, 1);
        assert_eq!(rows[0].1.timeout, 1);
        assert_eq!(rows[1].0, "zeta.delete");
        assert_eq!(rows[1].1.killed, 1);
    }

    #[test]
    fn mutator_yield_of_an_empty_population_is_empty() {
        // No mutator should be invented when discovery found no work.
        assert!(yield_by_mutator(&[]).is_empty());
    }
}
