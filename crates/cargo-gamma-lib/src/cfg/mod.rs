//! Deciding which conditionally compiled code is actually in the build.
//!
//! Rust strips `#[cfg(...)]` items before anything else runs, so code behind a predicate that does
//! not hold is not in the compiled artifact at all. A mutant placed there is therefore unkillable
//! by construction: activating it changes nothing, every test passes, and the tool reports a
//! survivor no test could ever have caught.
//!
//! That is not merely a wrong number. Each such mutant costs a full run of every test binary that
//! links its package, so a workspace with a lot of platform-specific or feature-gated code spends
//! most of its time proving things about code it did not build. On one real workspace, 378 of
//! 2,290 survivors — 16.5% — sat behind a gate that did not hold.
//!
//! # What this module decides
//!
//! [`CfgSet`] holds the configuration predicates that are true for the build, and answers whether a
//! given `#[cfg(...)]` attribute holds:
//!
//! ```rust
//! # use cargo_gamma_lib::cfg::CfgSet;
//! let set = CfgSet::parse("unix\ntarget_arch=\"x86_64\"\n").with_features(["std".to_owned()]);
//!
//! assert!(set.holds_str("unix"));
//! assert!(set.holds_str("feature = \"std\""));
//! assert!(!set.holds_str("windows"));
//! assert!(!set.holds_str("feature = \"stats\""));
//! assert!(set.holds_str("any(unix, windows)"));
//! assert!(set.holds_str("not(windows)"));
//! assert!(!set.holds_str("all(unix, feature = \"stats\")"));
//! ```
//!
//! The names and values come from `rustc --print cfg`, which is the compiler's own answer for the
//! target it is about to build, so `target_arch`, `target_os`, `unix`, `windows`, `panic` and any
//! `--cfg` passed through `RUSTFLAGS` are all covered without this module knowing what they mean.
//!
//! Features are the one thing `rustc` cannot answer, because they are Cargo's concept. They are
//! resolved separately, per package, by [`features`].
//!
//! # Erring toward keeping a mutant
//!
//! Every uncertainty resolves toward the predicate holding, which keeps the mutant. A mutant that
//! should not exist is visible and annoying; a mutant silently missing from the population is a
//! hole in the measurement that nobody can see. So an unparsable attribute, an unrecognised
//! predicate function, and a package whose features could not be resolved all leave the code
//! mutable:
//!
//! ```rust
//! # use cargo_gamma_lib::cfg::CfgSet;
//! let set = CfgSet::parse("unix\n");
//!
//! // `version` is a predicate this module does not model, so it is assumed to hold.
//! assert!(set.holds_str("version(\"1.80\")"));
//!
//! // And a set that was never resolved holds everything.
//! assert!(CfgSet::unconditional().holds_str("windows"));
//! ```

pub mod features;

use crate::error::error;
use crate::{HashMap, HashSet, Result};
use std::process::Command;
use syn::punctuated::Punctuated;
use syn::{Attribute, Expr, Lit, Meta, Token};

/// The configuration predicates that hold for a build.
///
/// Construct one with [`CfgSet::host`] for a real run, [`CfgSet::parse`] from captured `rustc`
/// output, or [`CfgSet::unconditional`] for a context where nothing should be stripped.
#[derive(Clone, Debug, Default)]
pub struct CfgSet {
    /// Bare names, such as `unix` or a `--cfg loom` passed through `RUSTFLAGS`.
    names: HashSet<String>,

    /// Key/value pairs, such as `target_arch="x86_64"` or `feature="std"`.
    pairs: HashSet<(String, String)>,

    /// Whether predicates are checked at all.
    ///
    /// An unresolved set holds everything, so a caller with no cfg information behaves exactly as
    /// this tool did before cfg evaluation existed.
    enforced: bool,
}

impl CfgSet {
    /// Returns a set under which every predicate holds, so nothing is ever stripped.
    #[must_use]
    pub fn unconditional() -> Self {
        Self::default()
    }

    /// Asks `rustc` which predicates hold for the host.
    ///
    /// `RUSTFLAGS` is inherited unchanged, because a `--cfg` there is exactly the kind of custom
    /// predicate — `loom`, `fuzzing`, `kani` — that gates code this tool would otherwise report
    /// survivors in.
    ///
    /// # Errors
    ///
    /// Returns an error if `rustc` cannot be run or answers with a failure.
    pub fn host() -> Result<Self> {
        let program = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
        let mut command = Command::new(program);

        let _builder = command.args(["--print", "cfg"]);

        let output = command
            .output()
            .map_err(|cause| error!("could not run `rustc --print cfg`").caused_by(cause))?;

        if !output.status.success() {
            return Err(error!(
                "`rustc --print cfg` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(Self::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Reads a set out of the lines `rustc --print cfg` prints.
    ///
    /// Each line is either a bare name or `key="value"`. Anything else is skipped rather than
    /// guessed at.
    ///
    /// ```rust
    /// # use cargo_gamma_lib::cfg::CfgSet;
    /// let set = CfgSet::parse("unix\ntarget_os=\"linux\"\n");
    ///
    /// assert!(set.holds_str("unix"));
    /// assert!(set.holds_str("target_os = \"linux\""));
    /// ```
    #[must_use]
    pub fn parse(printed: &str) -> Self {
        let mut set = Self { enforced: true, ..Self::default() };

        for line in printed.lines() {
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');

                if !key.is_empty() && !value.is_empty() {
                    let _added = set.pairs.insert((key.to_owned(), value.to_owned()));
                }
            } else {
                let _added = set.names.insert(line.to_owned());
            }
        }

        set
    }

    /// Adds the Cargo features enabled for the package this set will be used on.
    ///
    /// Features are per package, so one set per package is built from one shared `rustc` answer.
    #[must_use]
    pub fn with_features(mut self, features: impl IntoIterator<Item = String>) -> Self {
        for feature in features {
            let _added = self.pairs.insert(("feature".to_owned(), feature));
        }

        self
    }

    /// Returns whether this set was resolved, and therefore whether it strips anything.
    #[must_use]
    pub const fn is_enforced(&self) -> bool {
        self.enforced
    }

    /// Returns whether every `#[cfg(...)]` among `attrs` holds.
    ///
    /// Attributes other than `cfg` are ignored, and `cfg_attr` is deliberately not consulted: it
    /// adds attributes conditionally, it does not remove the item.
    #[must_use]
    pub fn holds_for(&self, attrs: &[Attribute]) -> bool {
        if !self.enforced {
            return true;
        }

        attrs.iter().all(|attribute| {
            if attribute.path().is_ident("cfg") {
                // An attribute this module cannot parse says nothing about whether the code is
                // built, so the code stays mutable.
                attribute.parse_args::<Meta>().map_or(true, |meta| self.holds(&meta))
            } else {
                true
            }
        })
    }

    /// Returns whether a predicate written as source text holds.
    ///
    /// An unparsable predicate holds, for the same reason an unparsable attribute does.
    #[must_use]
    pub fn holds_str(&self, predicate: &str) -> bool {
        syn::parse_str::<Meta>(predicate).map_or(true, |meta| self.holds(&meta))
    }

    /// Returns whether one parsed predicate holds, treating an unknown answer as holding.
    fn holds(&self, meta: &Meta) -> bool {
        !matches!(self.decide(meta), Verdict::No)
    }

    /// Decides one parsed predicate, which may be unanswerable.
    ///
    /// The three-valued answer is not pedantry. `not(version("1.80"))` has to come out *unknown*
    /// rather than false, because negating a predicate this module cannot evaluate would remove
    /// code from the population on the strength of a guess.
    fn decide(&self, meta: &Meta) -> Verdict {
        if !self.enforced {
            return Verdict::Unknown;
        }

        match meta {
            // A bare name: `unix`, `loom`, `test`. `rustc` lists every name that is on, so a name
            // that is absent is genuinely off rather than merely unrecognised.
            Meta::Path(path) => path.get_ident().map_or(Verdict::Unknown, |name| {
                Verdict::from(self.names.contains(&name.to_string()))
            }),

            // `key = "value"`: `target_arch = "x86_64"`, `feature = "std"`.
            Meta::NameValue(pair) => {
                let (Some(key), Expr::Lit(literal)) = (pair.path.get_ident(), &pair.value) else {
                    return Verdict::Unknown;
                };

                let Lit::Str(value) = &literal.lit else {
                    return Verdict::Unknown;
                };

                Verdict::from(self.pairs.contains(&(key.to_string(), value.value())))
            }

            Meta::List(list) => self.decide_list(list),
        }
    }

    /// Decides an `all(..)`, `any(..)` or `not(..)` predicate.
    fn decide_list(&self, list: &syn::MetaList) -> Verdict {
        let Some(name) = list.path.get_ident().map(ToString::to_string) else {
            return Verdict::Unknown;
        };

        // `version(..)`, and whatever the language adds next, is not modelled here, and an
        // unmodelled predicate must not remove code from the population.
        if !matches!(name.as_str(), "all" | "any" | "not") {
            return Verdict::Unknown;
        }

        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;

        let Ok(inner) = list.parse_args_with(parser) else {
            return Verdict::Unknown;
        };

        let mut answers = inner.iter().map(|meta| self.decide(meta));

        match name.as_str() {
            // One `No` settles an `all`, and one `Yes` settles an `any`, however unknown the rest
            // is. Otherwise an unknown among them leaves the whole thing unknown.
            "all" => combine(&mut answers, Verdict::No, Verdict::Yes),
            "any" => combine(&mut answers, Verdict::Yes, Verdict::No),

            // `not` takes exactly one predicate. Anything else is malformed, and a malformed
            // predicate is as unanswerable as an unmodelled one.
            _ => match inner.first() {
                Some(only) if inner.len() == 1 => self.decide(only).negated(),
                _ => Verdict::Unknown,
            },
        }
    }
}

/// What this module can say about a predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    /// The predicate holds, so the code is in the build.
    Yes,

    /// The predicate does not hold, so the compiler strips the code.
    No,

    /// This module cannot tell, so the code is left mutable.
    Unknown,
}

impl From<bool> for Verdict {
    fn from(held: bool) -> Self {
        if held { Self::Yes } else { Self::No }
    }
}

impl Verdict {
    /// Returns the answer to the negation of whatever produced this one.
    const fn negated(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// Folds the answers of a combinator's operands.
///
/// `settles` is the answer that decides the whole combinator on its own — `No` for `all`, `Yes`
/// for `any` — and `otherwise` is what an operand-free or entirely undecisive list comes to.
fn combine(answers: &mut dyn Iterator<Item = Verdict>, settles: Verdict, otherwise: Verdict) -> Verdict {
    let mut unknown = false;

    for answer in answers {
        if answer == settles {
            return settles;
        }

        unknown |= answer == Verdict::Unknown;
    }

    if unknown { Verdict::Unknown } else { otherwise }
}

/// One [`CfgSet`] per package, since features differ between packages but the target does not.
#[derive(Clone, Debug, Default)]
pub struct Cfgs {
    per_package: HashMap<String, CfgSet>,
    fallback: CfgSet,
}

impl Cfgs {
    /// Builds a set for every package named in `features`, sharing one target answer.
    #[must_use]
    pub fn new(target: &CfgSet, features: &HashMap<String, Vec<String>>) -> Self {
        let per_package = features
            .iter()
            .map(|(package, enabled)| (package.clone(), target.clone().with_features(enabled.iter().cloned())))
            .collect();

        Self { per_package, fallback: CfgSet::unconditional() }
    }

    /// Returns a map under which nothing is stripped, for callers with no cfg information.
    #[must_use]
    pub fn unconditional() -> Self {
        Self::default()
    }

    /// Returns the set for a package.
    ///
    /// A package that was never resolved gets the unconditional set, so an unexpected name leaves
    /// its code mutable rather than silently emptying it.
    #[must_use]
    pub fn for_package(&self, package: &str) -> &CfgSet {
        self.per_package.get(package).unwrap_or(&self.fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> CfgSet {
        CfgSet::parse("unix\ntarget_arch=\"x86_64\"\ntarget_os=\"linux\"\npanic=\"unwind\"\n")
            .with_features(["std".to_owned(), "serde".to_owned()])
    }

    fn attribute(text: &str) -> Vec<Attribute> {
        let item: syn::ItemFn = syn::parse_str(&format!("{text} fn f() {{}}")).expect("the fixture parses");

        item.attrs
    }

    #[test]
    fn a_bare_name_is_looked_up() {
        assert!(set().holds_str("unix"));
        assert!(!set().holds_str("windows"));
        assert!(!set().holds_str("loom"), "a custom cfg nobody passed is off");
    }

    #[test]
    fn a_key_value_pair_is_looked_up() {
        assert!(set().holds_str("target_arch = \"x86_64\""));
        assert!(!set().holds_str("target_arch = \"aarch64\""));
        assert!(set().holds_str("panic = \"unwind\""));
    }

    #[test]
    fn a_feature_is_a_pair_like_any_other() {
        assert!(set().holds_str("feature = \"std\""));
        assert!(!set().holds_str("feature = \"stats\""));
    }

    #[test]
    fn the_combinators_compose() {
        assert!(set().holds_str("all(unix, target_arch = \"x86_64\")"));
        assert!(!set().holds_str("all(unix, windows)"));
        assert!(set().holds_str("any(windows, unix)"));
        assert!(!set().holds_str("any(windows, feature = \"stats\")"));
        assert!(set().holds_str("not(windows)"));
        assert!(!set().holds_str("not(unix)"));
        assert!(set().holds_str("not(all(unix, windows))"));
        assert!(!set().holds_str("any()"), "an empty `any` holds for nothing");
        assert!(set().holds_str("all()"), "an empty `all` holds vacuously");
    }

    #[test]
    fn an_unmodelled_predicate_holds() {
        // Removing code because this module has not heard of a predicate would silently shrink the
        // population, which is the one failure mode nobody can see in a report.
        assert!(set().holds_str("version(\"1.80\")"));
        assert!(set().holds_str("not(version(\"1.80\"))"));
        assert!(set().holds_str("this is not valid syntax at all"));
    }

    #[test]
    fn an_unknown_operand_leaves_a_combinator_unknown() {
        // `not` of something unanswerable is unanswerable, not false. Getting this wrong would
        // strip code on the strength of a predicate this module has never heard of.
        assert!(set().holds_str("not(version(\"1.80\"))"));
        assert!(set().holds_str("all(unix, version(\"1.80\"))"));
        assert!(set().holds_str("any(windows, version(\"1.80\"))"));

        // A decisive operand still settles it, however unknown its neighbours are.
        assert!(!set().holds_str("all(windows, version(\"1.80\"))"));
        assert!(set().holds_str("any(unix, version(\"1.80\"))"));
        assert!(!set().holds_str("not(any(unix, version(\"1.80\")))"));
    }

    #[test]
    fn a_malformed_not_holds() {
        assert!(set().holds_str("not(unix, windows)"), "a `not` of two things is not a `not`");
        assert!(set().holds_str("not()"));
    }

    #[test]
    fn an_unresolved_set_holds_everything() {
        let set = CfgSet::unconditional();

        assert!(!set.is_enforced());
        assert!(set.holds_str("windows"));
        assert!(set.holds_str("feature = \"nothing-like-this\""));
        assert!(set.holds_str("not(unix)"));
        assert!(set.holds_for(&attribute("#[cfg(windows)]")));
    }

    #[test]
    fn printed_lines_that_are_not_settings_are_skipped() {
        let set = CfgSet::parse("unix\n\n   \nnonsense=\n=orphan\n");

        assert!(set.holds_str("unix"));
        assert!(!set.holds_str("nonsense = \"\""));
        assert!(!set.holds_str("orphan"));
    }

    #[test]
    fn only_cfg_attributes_are_consulted() {
        assert!(set().holds_for(&attribute("#[inline]")));
        assert!(set().holds_for(&attribute("#[doc = \"windows\"]")));

        // `cfg_attr` adds an attribute conditionally; it never removes the item.
        assert!(set().holds_for(&attribute("#[cfg_attr(windows, inline)]")));
    }

    #[test]
    fn every_cfg_attribute_has_to_hold() {
        assert!(set().holds_for(&attribute("#[cfg(unix)]")));
        assert!(!set().holds_for(&attribute("#[cfg(windows)]")));
        assert!(set().holds_for(&attribute("#[cfg(unix)]\n#[cfg(target_os = \"linux\")]")));
        assert!(!set().holds_for(&attribute("#[cfg(unix)]\n#[cfg(windows)]")));
    }

    #[test]
    fn an_unparsable_cfg_attribute_holds() {
        // A bare literal is not a `Meta`, so this is the shape that fails to parse.
        assert!(set().holds_for(&attribute("#[cfg(\"windows\")]")));
    }

    #[test]
    fn a_qualified_predicate_holds() {
        // A path with more than one segment is not something `cfg` accepts, so nothing is known
        // about it and the code stays mutable.
        assert!(set().holds_str("some::thing"));
        assert!(set().holds_str("some::thing(unix)"));
        assert!(set().holds_str("some::thing = \"x\""));
    }

    #[test]
    fn the_host_answers_about_itself() {
        let set = CfgSet::host().expect("rustc is on the path, since this test was compiled by it");

        assert!(set.is_enforced());
        assert!(set.holds_str("target_pointer_width = \"64\"") || set.holds_str("target_pointer_width = \"32\""));
        assert!(!set.holds_str("target_arch = \"there-is-no-such-architecture\""));
    }

    #[test]
    fn a_package_gets_its_own_features() {
        let mut features: HashMap<String, Vec<String>> = HashMap::default();

        let _old = features.insert("alpha".to_owned(), vec!["std".to_owned()]);
        let _old = features.insert("beta".to_owned(), Vec::new());

        let cfgs = Cfgs::new(&CfgSet::parse("unix\n"), &features);

        assert!(cfgs.for_package("alpha").holds_str("feature = \"std\""));
        assert!(!cfgs.for_package("beta").holds_str("feature = \"std\""));
        assert!(cfgs.for_package("alpha").holds_str("unix"));
    }

    #[test]
    fn an_unknown_package_is_left_alone() {
        let cfgs = Cfgs::new(&CfgSet::parse("unix\n"), &HashMap::default());

        assert!(!cfgs.for_package("nobody").is_enforced());
        assert!(cfgs.for_package("nobody").holds_str("windows"));
    }

    #[test]
    fn the_unconditional_map_strips_nothing() {
        let cfgs = Cfgs::unconditional();

        assert!(cfgs.for_package("anything").holds_str("windows"));
    }
}
