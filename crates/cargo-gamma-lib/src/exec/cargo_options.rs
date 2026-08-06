use core::time::Duration;

/// How cargo and the test binaries are invoked.
///
/// A run builds once and then executes that build thousands of times, so these are the settings
/// that decide what gets compiled and what the compiled thing is asked to do.
#[derive(Debug, Clone, Default)]
pub struct CargoOptions {
    /// Feature arguments, already rendered in the form cargo accepts.
    pub features: Vec<String>,

    /// The cargo profile to build with.
    pub profile: Option<String>,

    /// Extra arguments appended to every cargo invocation.
    pub extra: Vec<String>,

    /// Extra arguments appended to every test binary's command line.
    pub test_args: Vec<String>,
}

impl CargoOptions {
    /// Appends the build-shaping arguments to a cargo command line.
    pub fn extend_build_args(&self, args: &mut Vec<String>) {
        args.extend(self.features.iter().cloned());

        if let Some(profile) = self.profile.as_ref() {
            args.push("--profile".to_owned());
            args.push(profile.clone());
        }

        args.extend(self.extra.iter().cloned());
    }
}

/// How many build-and-withdraw rounds are allowed before a run gives up.
///
/// Some mutants are speculative — replacing a function body with `Some(Default::default())` only
/// compiles when the type happens to implement `Default` — and rustc reports only the errors it
/// reaches before it stops, so a large tree can need many rounds to converge. The cost of a round is
/// a rebuild of a tree that is already warm, whereas the cost of stopping too early is a run that
/// cannot complete at all, so the limit is deliberately lopsided.
pub const DEFAULT_ROLLBACK_ROUNDS: u32 = 256;

/// Limits on how long the build may take.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildLimits {
    /// A fixed budget for the build, whatever it turns out to cost.
    pub timeout: Option<Duration>,

    /// The multiple of the first round's duration a later rollback round is allowed.
    ///
    /// Rollback rounds recompile the same tree with strictly fewer live mutants, so a round that
    /// takes far longer than the first is not converging.
    pub multiplier: Option<f64>,

    /// How many build-and-withdraw rounds are allowed before the run gives up.
    ///
    /// Zero means the built-in default, so that a caller that does not care about rollback does not
    /// have to know what the default is.
    pub rollback_rounds: u32,
}

impl BuildLimits {
    /// Returns how many build-and-withdraw rounds are allowed.
    #[must_use]
    pub const fn rounds(&self) -> u32 {
        if self.rollback_rounds == 0 { DEFAULT_ROLLBACK_ROUNDS } else { self.rollback_rounds }
    }

    /// Returns the budget for a round, given how long the first round took.
    #[must_use]
    pub fn budget(&self, first: Option<Duration>) -> Option<Duration> {
        let scaled = self
            .multiplier
            .zip(first)
            .map(|(multiplier, first)| first.mul_f64(multiplier).max(MINIMUM_BUILD_BUDGET));

        match (self.timeout, scaled) {
            (Some(fixed), Some(scaled)) => Some(fixed.min(scaled)),
            (fixed, scaled) => fixed.or(scaled),
        }
    }
}

/// Floor under a scaled build budget, so a first round that finished instantly cannot produce one
/// that the next round trips over for reasons of scheduling alone.
const MINIMUM_BUILD_BUDGET: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rollback_rounds_means_the_default() {
        // A caller that does not care about rollback should not have to know what the default is,
        // and a build that allowed zero rounds could never even try once.
        assert_eq!(BuildLimits::default().rounds(), DEFAULT_ROLLBACK_ROUNDS);
        assert_eq!(BuildLimits { rollback_rounds: 7, ..BuildLimits::default() }.rounds(), 7);
    }

    #[test]
    fn no_limits_means_no_budget() {
        assert_eq!(BuildLimits::default().budget(Some(Duration::from_secs(10))), None);
    }

    #[test]
    fn a_fixed_timeout_applies_from_the_first_round() {
        let limits = BuildLimits { timeout: Some(Duration::from_secs(60)), multiplier: None, rollback_rounds: 0 };

        assert_eq!(limits.budget(None), Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_multiplier_needs_a_first_round_to_scale_from() {
        let limits = BuildLimits { timeout: None, multiplier: Some(2.0), rollback_rounds: 0 };

        assert_eq!(limits.budget(None), None);
        assert_eq!(limits.budget(Some(Duration::from_secs(100))), Some(Duration::from_secs(200)));
    }

    #[test]
    fn a_scaled_budget_never_falls_below_the_floor() {
        let limits = BuildLimits { timeout: None, multiplier: Some(2.0), rollback_rounds: 0 };

        assert_eq!(limits.budget(Some(Duration::from_secs(1))), Some(MINIMUM_BUILD_BUDGET));
    }

    #[test]
    fn the_tighter_of_the_two_wins() {
        let limits = BuildLimits { timeout: Some(Duration::from_secs(60)), multiplier: Some(2.0), rollback_rounds: 0 };

        assert_eq!(limits.budget(Some(Duration::from_secs(100))), Some(Duration::from_secs(60)));
    }

    #[test]
    fn build_args_are_rendered_in_cargo_order() {
        let options = CargoOptions {
            features: vec!["--all-features".to_owned()],
            profile: Some("release".to_owned()),
            extra: vec!["--offline".to_owned()],
            test_args: Vec::new(),
        };

        let mut args = Vec::new();

        options.extend_build_args(&mut args);

        assert_eq!(args, vec!["--all-features", "--profile", "release", "--offline"]);
    }
}
