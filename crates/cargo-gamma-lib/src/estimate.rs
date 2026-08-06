//! Projecting what a run will cost, before paying for it.
//!
//! The failure mode this exists to prevent is discovering a four-hour job four hours in. Everything
//! here is derived from measurements a run has already taken by the time the first mutant would
//! start: the build really built, the baseline really ran, and unviable mutants were really
//! withdrawn. Only one quantity is genuinely unknown before mutants execute — how much of the suite
//! a killed mutant gets through before something fails — and the projection says which assumption
//! it made about it rather than folding it silently into a single confident number.

use core::time::Duration;

use crate::advise::human;
use crate::model::{Mutant, Outcome};
use crate::report::quantity;

/// The share of the suite a killed mutant is assumed to reach before a test fails.
///
/// A killed mutant almost never runs the whole suite: something fails, and with a fail-fast binary
/// the rest is never reached. Assuming the full baseline for every mutant would overestimate badly
/// on a healthy codebase, which is the failure mode that makes an estimate useless — nobody plans
/// against a number they have learned is always too big. This is deliberately conservative.
const KILLED_SHARE: f64 = 0.60;

/// How wide the error bar is, as a fraction either side.
const ERROR_BAR: f64 = 0.25;

/// A projection of what a run will cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    /// Mutants that would actually be tested.
    pub live: usize,

    /// Mutants withdrawn during the build because they could not compile.
    pub withdrawn: usize,

    /// Measured: the instrumented build.
    pub build: Duration,

    /// Measured: the suite with no mutant active.
    pub baseline: Duration,

    /// Projected: testing every live mutant, at the configured parallelism.
    pub mutants: Duration,

    /// How many mutants are tested at once.
    pub jobs: usize,

    /// Projected: every live mutant running out the budget of every binary that can reach it.
    pub worst: Duration,
}

impl Estimate {
    /// The whole run, measured plus projected.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.build + self.baseline + self.mutants
    }

    /// The lower end of the error bar.
    #[must_use]
    pub fn low(&self) -> Duration {
        self.build + self.baseline + self.mutants.mul_f64(1.0 - ERROR_BAR)
    }

    /// The upper end of the error bar.
    ///
    /// Capped at the worst case, which is a real ceiling rather than a projection: a range whose
    /// top is above the point where every mutant has already run out of every second it will ever
    /// be given is describing time that cannot be spent.
    #[must_use]
    pub fn high(&self) -> Duration {
        (self.build + self.baseline + self.mutants.mul_f64(1.0 + ERROR_BAR)).min(self.worst_case())
    }

    /// The share of the run that is fixed cost, as a percentage.
    #[must_use]
    pub fn fixed_share(&self) -> f64 {
        let total = self.total().as_secs_f64();

        if total <= 0.0 {
            return 0.0;
        }

        (self.build.as_secs_f64() + self.baseline.as_secs_f64()) / total * 100.0
    }

    /// The worst case: every mutant hits its timeout and is then confirmed.
    ///
    /// Not a prediction — it is the number to plan a CI budget against, because a job killed at the
    /// hour mark produces no report at all. It includes the confirmation run, which is what makes
    /// it an actual ceiling: a mutant that exhausts its budget is not believed on the first try,
    /// so the path that costs the most costs several times its timeout rather than one of them.
    #[must_use]
    pub fn worst_case(&self) -> Duration {
        self.build + self.baseline + self.worst
    }
}

/// Projects a run from what the build and baseline already measured.
///
/// `reachable` and `budget` are serial totals over the live mutants, counting for each one only the
/// test binaries that can actually reach its package: what testing them all once would cost, and
/// what it would cost if every one of them timed out. Projecting from the whole baseline instead —
/// as though every mutant ran the entire suite — overestimates a loosely coupled workspace by
/// roughly the number of crates in it, which is exactly the shape of estimate nobody plans against
/// because they have learned it is always too big.
#[must_use]
pub fn project(mutants: &[Mutant], reachable: Duration, budget: Duration, baseline: Duration, build: Duration, jobs: usize) -> Estimate {
    let live = mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .count();

    let withdrawn = mutants.iter().filter(|mutant| mutant.outcome == Outcome::CompileError).count();
    let lanes = u32::try_from(jobs.max(1)).unwrap_or(1);

    Estimate {
        live,
        withdrawn,
        build,
        baseline,
        mutants: reachable.mul_f64(KILLED_SHARE) / lanes,
        jobs,
        worst: budget.saturating_mul(1 + crate::exec::CONFIRM_FACTOR) / lanes,
    }
}

/// Renders a projection as the single line printed once the fixed cost is paid.
///
/// One line, because it is printed in the middle of a run whose build and baseline timings are
/// already on the screen directly above it; repeating them would be padding. What is left is the
/// only thing the reader cannot already see: how long the remaining wait is, and how bad it could
/// get.
#[must_use]
pub fn render(estimate: &Estimate) -> String {
    format!(
        "{} to {} for {} at {}, {} worst case",
        human(estimate.low()),
        human(estimate.high()),
        quantity(estimate.live, "mutant"),
        quantity(estimate.jobs, "job"),
        human(estimate.worst_case())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::collect::Shape;
    use camino::Utf8PathBuf;

    fn mutant(ordinal: u32, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("m{ordinal}{outcome}"),
            ordinal,
            package: "p".to_owned(),
            mutator: "relational.lt_to_le".to_owned(),
            file: Utf8PathBuf::from("a.rs"),
            line: 1,
            column: 1,
            span: 0..1,
            item_path: "f".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a".to_owned(),
            replacement: "b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    fn population() -> Vec<Mutant> {
        let mut mutants: Vec<Mutant> = (1..=100).map(|index| mutant(index, Outcome::Pending)).collect();

        mutants.push(mutant(0, Outcome::Ignored));
        mutants.push(mutant(101, Outcome::CompileError));
        mutants
    }

    /// A serial workload of `secs` seconds, and a worst case ten times as bad.
    fn work(secs: u64) -> (Duration, Duration) {
        (Duration::from_secs(secs), Duration::from_secs(secs * 10))
    }

    #[test]
    fn only_mutants_that_would_run_are_counted() {
        let (reachable, budget) = work(100);
        let estimate = project(&population(), reachable, budget, Duration::ZERO, Duration::from_secs(5), 1);

        assert_eq!(estimate.live, 100);
        assert_eq!(estimate.withdrawn, 1);
    }

    #[test]
    fn parallelism_divides_the_projection() {
        let (reachable, budget) = work(1000);
        let one = project(&population(), reachable, budget, Duration::ZERO, Duration::from_secs(50), 1);
        let eight = project(&population(), reachable, budget, Duration::ZERO, Duration::from_secs(50), 8);

        assert_eq!(one.mutants / 8, eight.mutants);
        assert_eq!(one.worst / 8, eight.worst);
    }

    #[test]
    fn zero_jobs_does_not_divide_by_zero() {
        let (reachable, budget) = work(1000);
        let estimate = project(&population(), reachable, budget, Duration::ZERO, Duration::from_secs(50), 0);

        assert!(estimate.mutants > Duration::ZERO);
    }

    #[test]
    fn only_the_binaries_a_mutant_reaches_are_charged_for_it() {
        // The caller sums the reachable suites; a mutant that can only be seen by a tenth of the
        // workspace must cost a tenth of what one visible to all of it costs.
        let narrow = project(&population(), Duration::from_secs(100), Duration::ZERO, Duration::ZERO, Duration::ZERO, 1);
        let wide = project(&population(), Duration::from_secs(1000), Duration::ZERO, Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(narrow.mutants * 10, wide.mutants);
    }

    #[test]
    fn the_error_bar_brackets_the_estimate() {
        let (reachable, budget) = work(1000);
        let estimate = project(&population(), reachable, budget, Duration::from_secs(3), Duration::from_secs(50), 4);

        assert!(estimate.low() < estimate.total());
        assert!(estimate.total() < estimate.high());
    }

    #[test]
    fn the_error_bar_never_widens_the_part_that_was_measured() {
        // The build really happened; the projection has no business being uncertain about it.
        let estimate = project(&[], Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::from_secs(30), 4);

        assert_eq!(estimate.low(), Duration::from_secs(30));
        assert_eq!(estimate.high(), Duration::from_secs(30));
    }

    #[test]
    fn the_worst_case_exceeds_the_estimate() {
        let (reachable, budget) = work(1000);
        let estimate = project(&population(), reachable, budget, Duration::from_secs(3), Duration::from_secs(50), 4);

        assert!(estimate.worst_case() > estimate.high());
    }

    #[test]
    fn the_worst_case_pays_for_confirming_every_timeout() {
        // A mutant that runs out its budget is made to prove it, so a ceiling that counts one
        // timeout apiece is one a real run can walk straight past.
        let (reachable, budget) = work(1000);
        let estimate = project(&population(), reachable, budget, Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(estimate.worst_case(), budget.saturating_mul(1 + crate::exec::CONFIRM_FACTOR));
    }

    #[test]
    fn the_projected_range_never_reaches_past_the_ceiling() {
        // Above the worst case there is no time left to spend: every mutant has already been given
        // every second it will ever get.
        let (reachable, budget) = work(1);
        let estimate = project(&population(), reachable.mul_f64(1000.0), budget, Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(estimate.high(), estimate.worst_case());
    }

    #[test]
    fn a_dominant_build_shows_up_as_a_share() {
        let estimate = project(&[], Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::from_secs(30), 4);

        assert!((estimate.fixed_share() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_empty_run_has_no_share_rather_than_a_division_by_zero() {
        let estimate = project(&[], Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO, 4);

        assert!((estimate.fixed_share() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_rendering_is_one_line_carrying_the_range_the_population_and_the_worst_case() {
        let (reachable, budget) = work(1000);
        let estimate = project(&population(), reachable, budget, Duration::from_secs(3), Duration::from_secs(50), 4);
        let rendered = render(&estimate);

        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.contains(" to "), "{rendered}");
        assert!(rendered.contains("100 mutants"), "{rendered}");
        assert!(rendered.contains("4 jobs"), "{rendered}");
        assert!(rendered.contains("worst case"), "{rendered}");
    }
}
