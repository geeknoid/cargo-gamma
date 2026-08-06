use crate::HashSet;
use crate::Result;

use super::{REGISTRY, resolve};

/// The mutator that consumes caller-supplied error values.
const ERR_WITH: &str = "fn_value.err_with";

/// A resolved set of mutator names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    names: HashSet<&'static str>,
    errors: Vec<String>,
}

impl Selection {
    /// The set enabled when the user names nothing.
    #[must_use]
    pub fn default_profile() -> Self {
        Self {
            names: REGISTRY.iter().filter(|m| m.default_on).map(|m| m.name).collect(),
            errors: Vec::new(),
        }
    }

    /// Every registered mutator.
    #[must_use]
    pub fn everything() -> Self {
        Self {
            names: REGISTRY.iter().map(|m| m.name).collect(),
            errors: Vec::new(),
        }
    }

    /// An empty set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            names: HashSet::default(),
            errors: Vec::new(),
        }
    }

    /// Sets the caller-supplied `Err(...)` payloads that `fn_value.err_with` will use.
    ///
    /// `Err(Default::default())` only reaches error types that implement `Default`, which most
    /// hand-rolled error enums do not. Naming values here is how those functions get an error
    /// mutant at all, so supplying any also turns the mutator on.
    pub fn set_errors(&mut self, errors: Vec<String>) {
        if errors.is_empty() {
            self.errors = errors;
            return;
        }

        let _ = self.names.insert(ERR_WITH);
        self.errors = errors;
    }

    /// Removes the error mutator, for a selection the user spelled out without it.
    pub fn drop_errors(&mut self) {
        let _ = self.names.remove(ERR_WITH);
        self.errors.clear();
    }

    /// Returns the caller-supplied `Err(...)` payloads.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Returns whether a mutator is in the set.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Returns whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Returns the number of mutators in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns the names in sorted order.
    #[must_use]
    pub fn sorted(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.names.iter().copied().collect();

        names.sort_unstable();
        names
    }

    /// Adds every mutator matched by one selector.
    fn add(&mut self, selector: &str) -> Result<()> {
        for name in resolve(selector)? {
            let _ = self.names.insert(name);
        }

        Ok(())
    }

    /// Removes every mutator matched by one selector.
    fn remove(&mut self, selector: &str) -> Result<()> {
        for name in resolve(selector)? {
            let _ = self.names.remove(name);
        }

        Ok(())
    }

    /// Applies a comma-separated selector list to this set.
    ///
    /// Selectors are applied left to right, so a later `!family` can carve out of an earlier
    /// profile. A selector that matches nothing is an error, never a silent no-op: a suppression
    /// that quietly does nothing is the single most damaging failure mode a mutation tool can
    /// have, because the score stays high and nobody learns why.
    pub fn apply(&mut self, selectors: &str) -> Result<()> {
        for raw in selectors.split(',') {
            let selector = raw.trim();

            if selector.is_empty() {
                continue;
            }

            if let Some(rest) = selector.strip_prefix('!') {
                self.remove(rest.trim())?;
            } else {
                self.add(selector)?;
            }
        }

        Ok(())
    }

    /// Builds a selection from a selector list, starting from nothing.
    pub fn parse(selectors: &str) -> Result<Self> {
        let mut selection = Self::empty();

        selection.apply(selectors)?;
        Ok(selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_selectors_are_errors_not_silent_no_ops() {
        let error = resolve("nonsense_family").unwrap_err();

        assert!(error.to_string().contains("unknown mutator selector"));
    }

    #[test]
    fn negation_carves_out_of_a_profile() {
        let mut selection = Selection::parse("@arithmetic").unwrap();
        let before = selection.len();

        selection.apply("!bitwise").unwrap();

        assert!(selection.contains("arith.add_to_sub"));
        assert!(!selection.contains("bitwise.and_to_or"));
        assert!(selection.len() < before);
    }

    #[test]
    fn selectors_apply_left_to_right() {
        let selection = Selection::parse("relational, !relational.eq_to_ne").unwrap();

        assert!(selection.contains("relational.lt_to_le"));
        assert!(!selection.contains("relational.eq_to_ne"));
    }

    #[test]
    fn default_profile_is_everything() {
        let default = Selection::default_profile();
        let all = Selection::everything();

        // The catalog is deliberately on in full. A mutator that is present but unreachable
        // without a flag is one nobody runs, and a gap nobody sees is worse than a mutant somebody
        // has to judge, so the cost of the noisier families is paid rather than hidden.
        assert!(!default.is_empty());
        assert_eq!(default.len(), all.len());

        for name in all.sorted() {
            assert!(default.contains(name), "{name} is not reachable without a flag");
        }
    }

    #[test]
    fn the_noisier_families_are_still_on() {
        let default = Selection::default_profile();

        // Statement deletion has a high equivalent-mutant rate. It stays on anyway; this test
        // exists so that turning it off again is a deliberate edit rather than a quiet drift.
        assert!(default.contains("stmt.delete_call"));
    }

    #[test]
    fn empty_selectors_are_ignored() {
        let selection = Selection::parse("relational, , ").unwrap();

        assert_eq!(selection.len(), 10);
    }
}
