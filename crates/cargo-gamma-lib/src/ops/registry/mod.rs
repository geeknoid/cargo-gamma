//! The mutator registry: stable names, families, profiles, and selector resolution.
//!
//! Every mutator has one stable, well-known name of the form `family.transform`. That name is the
//! single vocabulary used by all three suppression channels, by `--ops`, by the report, and by
//! configuration. Nothing anywhere refers to a mutator by index, by description string, or by
//! position in a list, because all three of those change when the catalog grows.

mod mutator;
mod profile;
mod selection;

use crate::HashSet;
use crate::Result;
use crate::error::{Error, error};

pub use mutator::Mutator;
pub use profile::Profile;
pub use selection::Selection;

macro_rules! mutators {
    ($($name:literal, $default:literal, $aliases:expr, $description:literal;)+) => {
        /// Every mutator known to the tool, in registry order.
        pub const REGISTRY: &[Mutator] = &[
            $(Mutator { name: $name, description: $description, default_on: $default, aliases: $aliases },)+
        ];
    };
}

mutators! {
    // ---- Function value replacement. The cargo-mutants parity family. -----------------------
    "fn_value.default",          true,  &["RV"],  "replace the function body with a default value";
    "fn_value.unit",             true,  &[],      "replace the body of a unit function with ()";
    "fn_value.bool_true",        true,  &[],      "replace the body with true";
    "fn_value.bool_false",       true,  &[],      "replace the body with false";
    "fn_value.zero",             true,  &[],      "replace the body with 0";
    "fn_value.one",              true,  &[],      "replace the body with 1";
    "fn_value.minus_one",        true,  &[],      "replace the body with -1";
    "fn_value.empty_string",     true,  &[],      "replace the body with an empty string";
    "fn_value.xyzzy_string",     true,  &[],      "replace the body with a non-empty string";
    "fn_value.none",             true,  &[],      "replace the body with None";
    "fn_value.some_default",     true,  &[],      "replace the body with Some(Default::default())";
    "fn_value.ok_default",       true,  &[],      "replace the body with Ok(Default::default())";
    "fn_value.err_default",      true,  &[],      "replace the body with Err(Default::default())";
    "fn_value.err_with",         true , &[],      "replace the body with Err(v) for each --error value";
    "fn_value.two",              true,  &[],      "replace the body with 2";
    "fn_value.some",             true,  &[],      "replace the body with Some(value)";
    "fn_value.ok",               true,  &[],      "replace the body with Ok(value)";
    "fn_value.empty_collection", true,  &[],      "replace the body with an empty collection or iterator";
    "fn_value.one_element",      true,  &[],      "replace the body with a one-element collection or iterator";
    "fn_value.tuple",            true,  &[],      "replace the body with a tuple of replacement values";

    // ---- Relational and boundary. ------------------------------------------------------------
    "relational.lt_to_le",       true,  &["ROR"], "replace < with <=";
    "relational.lt_to_gt",       true,  &["ROR"], "replace < with >";
    "relational.le_to_lt",       true,  &["ROR"], "replace <= with <";
    "relational.le_to_ge",       true,  &["ROR"], "replace <= with >=";
    "relational.gt_to_ge",       true,  &["ROR"], "replace > with >=";
    "relational.gt_to_lt",       true,  &["ROR"], "replace > with <";
    "relational.ge_to_gt",       true,  &["ROR"], "replace >= with >";
    "relational.ge_to_le",       true,  &["ROR"], "replace >= with <=";
    "relational.eq_to_ne",       true,  &["ROR"], "replace == with !=";
    "relational.ne_to_eq",       true,  &["ROR"], "replace != with ==";

    // ---- Arithmetic. -------------------------------------------------------------------------
    "arith.add_to_sub",          true,  &["AOR"], "replace + with -";
    "arith.add_to_mul",          true , &["AOR"], "replace + with *";
    "arith.sub_to_add",          true,  &["AOR"], "replace - with +";
    "arith.sub_to_div",          true , &["AOR"], "replace - with /";
    "arith.mul_to_div",          true,  &["AOR"], "replace * with /";
    "arith.mul_to_add",          true , &["AOR"], "replace * with +";
    "arith.div_to_mul",          true,  &["AOR"], "replace / with *";
    "arith.div_to_rem",          true , &["AOR"], "replace / with %";
    "arith.rem_to_div",          true,  &["AOR"], "replace % with /";
    "arith.rem_to_mul",          true , &["AOR"], "replace % with *";

    // ---- Bitwise and shift. ------------------------------------------------------------------
    "bitwise.and_to_or",         true,  &["AOR"], "replace & with |";
    "bitwise.or_to_and",         true,  &["AOR"], "replace | with &";
    "bitwise.xor_to_and",        true,  &["AOR"], "replace ^ with &";
    "bitwise.and_to_xor",        true , &["AOR"], "replace & with ^";
    "shift.shl_to_shr",          true,  &["AOR"], "replace << with >>";
    "shift.shr_to_shl",          true,  &["AOR"], "replace >> with <<";

    // ---- Compound assignment. ----------------------------------------------------------------
    "assign.add_to_sub",         true,  &["ASR"], "replace += with -=";
    "assign.sub_to_add",         true,  &["ASR"], "replace -= with +=";
    "assign.mul_to_div",         true,  &["ASR"], "replace *= with /=";
    "assign.div_to_mul",         true,  &["ASR"], "replace /= with *=";
    "assign.rem_to_div",         true , &["ASR"], "replace %= with /=";
    "assign.and_to_or",          true,  &["ASR"], "replace &= with |=";
    "assign.or_to_and",          true,  &["ASR"], "replace |= with &=";
    "assign.xor_to_and",         true , &["ASR"], "replace ^= with &=";
    "assign.shl_to_shr",         true , &["ASR"], "replace <<= with >>=";
    "assign.shr_to_shl",         true , &["ASR"], "replace >>= with <<=";

    // ---- Logical and condition. --------------------------------------------------------------
    "logical.and_to_or",         true,  &["LCR"], "replace && with ||";
    "logical.or_to_and",         true,  &["LCR"], "replace || with &&";
    "cond.negate",               true,  &["COR"], "negate a branch condition";
    "cond.always_true",          true , &["COR"], "force a branch condition to true";
    "cond.always_false",         true , &["COR"], "force a branch condition to false";

    // ---- Match guards. The condition family's blind spot. --------------------------------------
    "match_guard.negate",        true,  &["COR"], "negate a match arm's guard";
    "match_guard.always_true",   true , &["COR"], "force a match arm's guard to true";
    "match_guard.always_false",  true , &["COR"], "force a match arm's guard to false";

    // ---- Match arms. -------------------------------------------------------------------------
    "match_arm.never_matches",   true , &["SDL"], "stop a match arm from matching, falling through to the wildcard";

    // ---- Struct literals. --------------------------------------------------------------------
    "struct_field.omit",         true , &["SDL"], "omit a struct literal field, leaving the base expression to supply it";

    // ---- Ranges. -----------------------------------------------------------------------------
    "range.exclusive_to_inclusive", true, &["ROR"], "extend a .. range to cover its endpoint";
    "range.inclusive_to_exclusive", true, &["ROR"], "shrink a ..= range to stop short of its endpoint";

    // ---- Loop control flow. ------------------------------------------------------------------
    "loop.break_to_continue",    true , &[],      "replace break with continue";
    "loop.continue_to_break",    true,  &[],      "replace continue with break";
    "loop.delete_break",         true , &["SDL"], "delete a break statement";
    "loop.delete_continue",      true , &["SDL"], "delete a continue statement";

    // ---- Unary. ------------------------------------------------------------------------------
    "unary.remove_neg",          true,  &["UOI"], "remove a unary minus";
    "unary.remove_not",          true,  &["UOI"], "remove a unary not";

    // ---- Literals. ---------------------------------------------------------------------------
    "literal.int_to_zero",       true,  &["CRP"], "replace an integer literal with 0";
    "literal.int_to_one",        true,  &["CRP"], "replace an integer literal with 1";
    "literal.int_increment",     true,  &["CRP"], "add one to an integer literal";
    "literal.int_decrement",     true,  &["CRP"], "subtract one from an integer literal";
    "literal.bool_flip",         true,  &["CRP"], "invert a boolean literal";
    "literal.str_to_empty",      true,  &["CRP"], "replace a string literal with an empty string";
    "literal.str_to_xyzzy",      true , &["CRP"], "replace a string literal with a different string";

    // ---- Statement deletion and side-effect removal. ------------------------------------------
    "stmt.delete_call",          true , &["SDL"], "delete a statement whose value is discarded";
    "stmt.delete_assign",        true , &["SDL"], "delete a compound assignment statement";

    // ---- Focused numeric perturbation, in boundary-sensitive positions only. -------------------
    "expr.increment",            true,  &["EVR"], "add one to a numeric expression in a boundary-sensitive position";
    "expr.decrement",            true,  &["EVR"], "subtract one from a numeric expression in a boundary-sensitive position";

    // ---- Option and Result construction. -------------------------------------------------------
    // These ask about error handling at the point it is decided, which whole-function replacement
    // can only ask about a function at a time.
    "option.some_to_none",       true,  &["EVR"], "replace Some(value) with None";
    "option.none_to_some",       true,  &["EVR"], "replace None with Some(Default::default())";
    "result.ok_to_err",          true,  &["EVR"], "replace Ok(value) with Err(Default::default())";
    "result.err_to_ok",          true,  &["EVR"], "replace Err(value) with Ok(Default::default())";

    // ---- Iterator quantifiers and selectors. ---------------------------------------------------
    // Limited to a curated set of standard-library names. Without type resolution there is no way
    // to know that a user's `take` means what the standard library's does, so the risk is applying
    // a transformation that is not the one advertised.
    "iter.any_to_all",           true,  &["EVR"], "replace any with all";
    "iter.all_to_any",           true,  &["EVR"], "replace all with any";
    "iter.min_to_max",           true,  &["EVR"], "replace min with max";
    "iter.max_to_min",           true,  &["EVR"], "replace max with min";
    "iter.first_to_last",        true,  &["EVR"], "replace first with last";
    "iter.last_to_first",        true,  &["EVR"], "replace last with first";
    "iter.remove_sort",          true,  &["SDL"], "remove a sort from a chain";
    "iter.remove_dedup",         true,  &["SDL"], "remove a deduplication from a chain";

    // ---- String semantics. ---------------------------------------------------------------------
    "string.starts_with_to_ends_with", true, &["EVR"], "replace starts_with with ends_with";
    "string.ends_with_to_starts_with", true, &["EVR"], "replace ends_with with starts_with";
    "string.lower_to_upper",     true,  &["EVR"], "replace to_lowercase with to_uppercase";
    "string.upper_to_lower",     true,  &["EVR"], "replace to_uppercase with to_lowercase";
    "string.trim_start_to_trim_end", true, &["EVR"], "replace trim_start with trim_end";
    "string.trim_end_to_trim_start", true, &["EVR"], "replace trim_end with trim_start";

    // ---- Collection construction. --------------------------------------------------------------
    // Only `vec![]`, never an array: an array's length is part of its type, so removing an element
    // changes the type rather than the behaviour.
    "collection.omit_element",   true,  &["SDL"], "omit an element from a vec! literal";

    // ---- Assignment values. --------------------------------------------------------------------
    "assign_value.default",      true,  &["EVR"], "replace an assigned value with its type's default";
}

/// Every profile known to the tool.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: "all",
        description: "every registered mutator",
        members: &["*"],
    },
    Profile {
        name: "default",
        description: "the mutators enabled when none are named, which is currently all of them",
        members: &["@default"],
    },
    Profile {
        name: "parity",
        description: "the cargo-mutants operator set, for the differential oracle",
        members: &["fn_value"],
    },
    Profile {
        name: "boundary",
        description: "relational and boundary conditions",
        members: &["relational", "range"],
    },
    Profile {
        name: "arithmetic",
        description: "arithmetic, bitwise, shift and compound assignment",
        members: &["arith", "bitwise", "shift", "assign"],
    },
    Profile {
        name: "logical",
        description: "logical operators and branch conditions",
        members: &["logical", "cond", "match_guard"],
    },
    Profile {
        name: "control",
        description: "the choices control flow makes: conditions, guards, arms and loop exits",
        members: &["cond", "match_guard", "match_arm", "loop"],
    },
    Profile {
        name: "removal",
        description: "statement and side-effect deletion",
        members: &["stmt", "unary", "match_arm", "struct_field", "collection"],
    },
    Profile {
        name: "semantics",
        description: "standard-library meaning: Option, Result, iterators, strings and collections",
        members: &["option", "result", "iter", "string", "collection", "assign_value"],
    },
    Profile {
        name: "literals",
        description: "literal and constant replacement",
        members: &["literal"],
    },
    Profile {
        name: "numeric",
        description: "literal replacement and focused numeric expression perturbation",
        members: &["literal", "expr"],
    },
    Profile {
        name: "extreme",
        description: "a synonym for `all`, kept because scripts name it",
        members: &["*"],
    },
];

/// Looks up a mutator by its exact registry name.
#[must_use]
pub fn find(name: &str) -> Option<&'static Mutator> {
    REGISTRY.iter().find(|mutator| mutator.name == name)
}

/// Looks up a profile by name, without the leading `@`.
#[must_use]
pub fn find_profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.name == name)
}

/// Returns the distinct family prefixes present in the registry, in registry order.
#[must_use]
pub fn families() -> Vec<&'static str> {
    let mut seen = HashSet::default();
    let mut out = Vec::new();

    for mutator in REGISTRY {
        let family = mutator.name.split('.').next().unwrap_or(mutator.name);

        if seen.insert(family) {
            out.push(family);
        }
    }

    out
}

/// Expands one selector into the mutator names it matches.
///
/// A selector is a full name, a family prefix, an `@profile`, an academic alias, or `all`.
pub fn resolve(selector: &str) -> Result<Vec<&'static str>> {
    if selector == "all" {
        return Ok(REGISTRY.iter().map(|m| m.name).collect());
    }

    if let Some(profile_name) = selector.strip_prefix('@') {
        let profile = find_profile(profile_name).ok_or_else(|| unknown(selector))?;
        let mut names = Vec::new();

        for member in profile.members {
            match *member {
                "*" => names.extend(REGISTRY.iter().map(|m| m.name)),
                "@default" => names.extend(REGISTRY.iter().filter(|m| m.default_on).map(|m| m.name)),
                other => names.extend(resolve(other)?),
            }
        }

        return Ok(names);
    }

    if let Some(mutator) = find(selector) {
        return Ok(vec![mutator.name]);
    }

    // Family or sub-family prefix: `arith`, or `combinator.iter`.
    let prefix = format!("{selector}.");
    let matched: Vec<&'static str> = REGISTRY
        .iter()
        .filter(|m| m.name.starts_with(prefix.as_str()))
        .map(|m| m.name)
        .collect();

    if !matched.is_empty() {
        return Ok(matched);
    }

    // Academic or industry alias, matched case-insensitively.
    let matched: Vec<&'static str> = REGISTRY
        .iter()
        .filter(|m| m.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(selector)))
        .map(|m| m.name)
        .collect();

    if matched.is_empty() { Err(unknown(selector)) } else { Ok(matched) }
}

/// Builds the error for an unmatched selector, with a spelling suggestion.
fn unknown(selector: &str) -> Error {
    let mut best: Option<(f64, String)> = None;

    let candidates = REGISTRY
        .iter()
        .map(|m| m.name.to_owned())
        .chain(families().into_iter().map(ToOwned::to_owned))
        .chain(REGISTRY.iter().flat_map(|m| m.aliases.iter().map(|a| (*a).to_owned())))
        .chain(PROFILES.iter().map(|p| format!("@{}", p.name)));

    for candidate in candidates {
        let score = strsim::jaro_winkler(selector, &candidate);

        if score > best.as_ref().map_or(0.85, |(previous, _)| *previous) {
            best = Some((score, candidate));
        }
    }

    best.map_or_else(
        || error!("unknown mutator selector `{selector}`; run `cargo gamma list ops` to see the registry"),
        |(_, suggestion)| error!("unknown mutator selector `{selector}`; did you mean `{suggestion}`?"),
    )
    .usage()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique() {
        let mut seen = HashSet::default();

        for mutator in REGISTRY {
            assert!(seen.insert(mutator.name), "duplicate mutator name {}", mutator.name);
        }
    }

    #[test]
    fn registry_names_are_family_dot_transform() {
        for mutator in REGISTRY {
            let parts: Vec<&str> = mutator.name.split('.').collect();

            assert_eq!(parts.len(), 2, "{} is not family.transform", mutator.name);
            assert!(!parts[0].is_empty() && !parts[1].is_empty(), "{}", mutator.name);
            assert!(
                parts
                    .iter()
                    .all(|p| p.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())),
                "{} is not snake_case",
                mutator.name
            );
        }
    }

    #[test]
    fn every_mutator_has_a_description() {
        for mutator in REGISTRY {
            assert!(!mutator.description.is_empty(), "{}", mutator.name);
        }
    }

    #[test]
    fn full_names_resolve_to_themselves() {
        assert_eq!(resolve("arith.add_to_sub").unwrap(), vec!["arith.add_to_sub"]);
    }

    #[test]
    fn family_prefixes_resolve_to_the_family() {
        let resolved = resolve("relational").unwrap();

        assert_eq!(resolved.len(), 10);
        assert!(resolved.contains(&"relational.lt_to_le"));
        assert!(!resolved.contains(&"arith.add_to_sub"));
    }

    #[test]
    fn profiles_resolve() {
        let arithmetic = resolve("@arithmetic").unwrap();

        assert!(arithmetic.contains(&"arith.add_to_sub"));
        assert!(arithmetic.contains(&"bitwise.and_to_or"));
        assert!(arithmetic.contains(&"shift.shl_to_shr"));
        assert!(!arithmetic.contains(&"relational.lt_to_le"));
    }

    #[test]
    fn aliases_resolve_case_insensitively() {
        let upper = resolve("ROR").unwrap();
        let lower = resolve("ror").unwrap();

        assert_eq!(upper, lower);
        assert!(upper.contains(&"relational.eq_to_ne"));
    }

    #[test]
    fn all_resolves_to_the_whole_registry() {
        assert_eq!(resolve("all").unwrap().len(), REGISTRY.len());
    }

    #[test]
    fn unknown_selectors_suggest_a_spelling() {
        let error = resolve("arith.add_to_subb").unwrap_err();

        assert!(error.to_string().contains("did you mean `arith.add_to_sub`?"), "{error}");
    }

    #[test]
    fn a_misspelled_profile_suggests_the_profile() {
        let error = resolve("@arithmetics").unwrap_err();

        assert!(error.to_string().contains("@arithmetic"), "{error}");
    }

    #[test]
    fn a_wildly_wrong_selector_points_at_the_registry() {
        let error = resolve("zzzzzzzzzzzz").unwrap_err();

        assert!(error.to_string().contains("cargo gamma list ops"), "{error}");
    }

    #[test]
    fn every_profile_resolves_and_is_non_empty() {
        for profile in PROFILES {
            let resolved = resolve(&format!("@{}", profile.name)).unwrap();

            assert!(!resolved.is_empty(), "profile @{} is empty", profile.name);
        }
    }

    #[test]
    fn families_are_listed_in_registry_order_without_duplicates() {
        let families = families();
        let mut seen = HashSet::default();

        for family in &families {
            assert!(seen.insert(*family), "duplicate family {family}");
        }

        assert_eq!(families.first().copied(), Some("fn_value"));
    }
}
