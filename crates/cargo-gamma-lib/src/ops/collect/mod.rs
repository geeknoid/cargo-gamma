//! Walking a syntax tree and producing the mutants it admits.
//!
//! The traversal tracks the enclosing item path and how many identical sites for a mutator have
//! already been seen, giving each mutant an identity that survives reformatting and code motion.

mod candidate;
mod collector;
mod shape;

use camino::Utf8PathBuf;
use syn::visit::Visit;

use crate::HashMap;
use crate::cfg::CfgSet;
use crate::model::{Mutant, Outcome, mutant_id, normalize_site_text, site_key};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

use collector::Collector;

pub use candidate::Candidate;
pub use shape::Shape;

/// Collects every candidate a file admits under the given selection, with nothing stripped for
/// configuration.
///
/// Equivalent to [`collect_in`] with an unconditional set. Use it where the build's configuration
/// is not known, which is every caller that is examining a fragment of source rather than a real
/// workspace.
///
/// The result is sorted by span start, then by mutator name, so that two runs over the same source
/// produce the same order regardless of how the traversal happened to visit siblings.
#[must_use]
pub fn collect(file: &SourceFile, selection: &Selection) -> Vec<Candidate> {
    collect_in(file, selection, &CfgSet::unconditional())
}

/// Collects every candidate a file admits, under a selection and a build configuration.
///
/// `cfg` decides which conditionally compiled code is actually in the build. Code behind a
/// predicate that does not hold produces no candidates at all: the compiler strips it, so a guard
/// there would never be compiled and its mutant could never be activated by any test.
#[must_use]
pub fn collect_in(file: &SourceFile, selection: &Selection, cfg: &CfgSet) -> Vec<Candidate> {
    let mut collector = Collector::new(file, selection, selection.errors(), cfg);

    collector.visit_file(&file.ast);

    let mut candidates = collector.finish();

    candidates.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| left.span.end.cmp(&right.span.end))
            .then_with(|| left.mutator.cmp(right.mutator))
            .then_with(|| left.replacement_index.cmp(&right.replacement_index))
    });

    candidates
}

/// Turns candidates into mutants, assigning stable ids.
///
/// Ordinals are left at zero here. They are a run-wide selector, so only the caller that has seen
/// every file can assign them.
#[must_use]
pub fn into_mutants(file: &SourceFile, package: &str, candidates: Vec<Candidate>) -> Vec<Mutant> {
    let mut occurrences: HashMap<u128, u32> = HashMap::default();
    let path = Utf8PathBuf::from(file.path.as_str());
    let mut mutants = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let original = file.slice(&candidate.span).to_owned();
        let normalized = normalize_site_text(&original);
        let occurrence = occurrences
            .entry(site_key(&candidate.item_path, candidate.mutator, &normalized))
            .or_insert(0);

        let index = *occurrence;

        *occurrence += 1;

        let (line, column) = file.location(candidate.span.start);

        mutants.push(Mutant {
            id: mutant_id(
                &file.path,
                &candidate.item_path,
                candidate.mutator,
                &normalized,
                index,
                candidate.replacement_index,
            ),
            ordinal: 0,
            file: path.clone(),
            package: package.to_owned(),
            span: candidate.span,
            line,
            column,
            mutator: candidate.mutator.to_owned(),
            item_path: candidate.item_path,
            occurrence: index,
            replacement_index: candidate.replacement_index,
            original,
            replacement: candidate.replacement,
            shape: candidate.shape,
            outcome: Outcome::Pending,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        });
    }

    mutants
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::registry::Selection;

    fn candidates(source: &str, ops: &str) -> Vec<Candidate> {
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let selection = Selection::parse(ops).unwrap();

        collect(&file, &selection)
    }

    fn mutators(source: &str, ops: &str) -> Vec<&'static str> {
        candidates(source, ops).into_iter().map(|c| c.mutator).collect()
    }

    fn with_errors(source: &str, errors: &[&str]) -> Vec<Candidate> {
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let mut selection = Selection::empty();

        selection.set_errors(errors.iter().map(|e| (*e).to_owned()).collect());
        collect(&file, &selection)
    }

    #[test]
    fn each_named_error_value_becomes_its_own_mutant() {
        let found = with_errors(
            "fn f() -> Result<i32, MyError> { Ok(1) }",
            &["MyError::Io", "MyError::Eof"],
        );

        let replacements: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

        assert_eq!(replacements, vec!["Err(MyError::Io)", "Err(MyError::Eof)"]);
        assert!(found.iter().all(|c| c.mutator == "fn_value.err_with"));
    }

    #[test]
    fn named_error_values_only_reach_functions_returning_result() {
        let found = with_errors("fn f() -> i32 { 1 }", &["MyError::Io"]);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn naming_no_error_values_produces_no_error_mutants() {
        let found = with_errors("fn f() -> Result<i32, MyError> { Ok(1) }", &[]);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_less_than_yields_both_relational_replacements() {
        let found = mutators("fn f(a: i32, b: i32) -> bool { a < b }", "relational");

        assert!(found.contains(&"relational.lt_to_le"));
        assert!(found.contains(&"relational.lt_to_gt"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn unselected_mutators_produce_nothing() {
        assert!(mutators("fn f(a: i32, b: i32) -> bool { a < b }", "arith").is_empty());
    }

    #[test]
    fn spans_cover_the_whole_binary_expression() {
        let source = "fn f(a: i32, b: i32) -> bool { a < b }";
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let found = collect(&file, &Selection::parse("relational").unwrap());

        assert_eq!(file.slice(&found[0].span), "a < b");
    }

    #[test]
    fn nested_binary_expressions_are_all_found() {
        let found = mutators("fn f(a: i32, b: i32, c: i32) -> i32 { a + b * c }", "arith");

        assert!(found.contains(&"arith.add_to_sub"));
        assert!(found.contains(&"arith.mul_to_div"));
    }

    #[test]
    fn candidates_come_back_in_source_order() {
        let source = "fn f(a: i32, b: i32) -> i32 { let x = a - b; x * a }";
        let found = candidates(source, "arith");

        for pair in found.windows(2) {
            assert!(pair.len() > 1);
            assert!(pair[0].span.start <= pair[1].span.start);
        }
    }

    #[test]
    fn the_largest_integer_literal_does_not_overflow() {
        // `i64::MAX` has no increment. Computing one unchecked panics in a debug build and wraps to
        // `i64::MIN` in a release build, which would offer a "+1" mutant that is smaller than the
        // literal it replaces.
        let found = mutators("fn f() -> i64 { 9223372036854775807 }", "literal");

        assert!(found.contains(&"literal.int_decrement"), "{found:?}");
        assert!(!found.contains(&"literal.int_increment"), "{found:?}");
    }

    #[test]
    fn item_paths_include_the_enclosing_function() {
        let found = candidates("fn outer(a: i32) -> i32 { a + 1 }", "arith.add_to_sub");

        assert_eq!(found[0].item_path, "outer");
    }

    #[test]
    fn item_paths_include_module_impl_and_method() {
        let source = "mod m { struct S; impl S { fn go(&self, a: i32) -> i32 { a + 1 } } }";
        let found = candidates(source, "arith.add_to_sub");

        assert_eq!(found[0].item_path, "m::S::go");
    }

    #[test]
    fn impl_paths_strip_references_and_generics() {
        let source = "struct S<T>(T); impl<T> S<T> { fn go(&self, a: i32) -> i32 { a + 1 } }";
        let found = candidates(source, "arith.add_to_sub");

        assert_eq!(found[0].item_path, "S::go");
    }

    #[test]
    fn test_functions_are_not_mutated() {
        let source = "#[test] fn t() { assert_eq!(1 + 1, 2); }";

        assert!(candidates(source, "arith").is_empty());
    }

    #[test]
    fn test_impl_methods_are_not_mutated() {
        let source = "struct S; impl S { #[test] fn t(&self) { let _ = 1 + 1; } }";

        assert!(candidates(source, "arith").is_empty());
    }

    #[test]
    fn cfg_test_modules_are_not_mutated() {
        let source = "#[cfg(test)] mod tests { fn helper(a: i32) -> i32 { a + 1 } }";

        assert!(candidates(source, "arith").is_empty());
    }

    #[test]
    fn non_test_cfg_modules_are_mutated() {
        let source = "#[cfg(unix)] mod platform { fn helper(a: i32) -> i32 { a + 1 } }";

        assert_eq!(candidates(source, "arith.add_to_sub").len(), 1);
    }

    #[test]
    fn tokio_test_functions_are_not_mutated() {
        let source = "#[tokio::test] async fn t() { let _ = 1 + 1; }";

        assert!(candidates(source, "arith").is_empty());
    }

    #[test]
    fn const_initializers_are_not_mutated() {
        // The encoding wraps expressions in an `if` over a function call, which const contexts
        // reject, so generating these would produce guaranteed compile failures.
        assert!(candidates("const N: i32 = 1 + 2;", "arith").is_empty());
        assert!(candidates("static N: i32 = 1 + 2;", "arith").is_empty());
    }

    #[test]
    fn const_fn_bodies_are_not_mutated() {
        // Every expression inside a `const fn` is in a const context, not just the body value, so
        // the whole subtree has to stay inert. Mutating one of these compiles nowhere.
        assert!(candidates("const fn f(a: i32, b: i32) -> i32 { a + b }", "arith").is_empty());
        assert!(candidates("const fn f(a: usize, b: &[u8]) -> bool { a < b.len() }", "relational").is_empty());

        // A non-const function in the same file must still be mutated.
        let source = "const fn f(a: i32) -> i32 { a + 1 }\nfn g(a: i32) -> i32 { a + 2 }";

        assert!(!candidates(source, "arith").is_empty());
    }

    #[test]
    fn array_lengths_in_types_are_not_mutated() {
        // `[u8; 200]` in a type is a const context, and the guard is a function call. This is not
        // the same position as the length in the *value* `[0u8; 32]`, which was already inert, and
        // the difference cost a real crate a build that could not compile and could not be blamed
        // on any one mutant.
        assert!(candidates("struct Pairs([u8; 200]);", "literal").is_empty());
        assert!(candidates("struct Pairs([u8; 100 * 2]);", "arith").is_empty());
        assert!(candidates("fn f() -> [u8; 4] { todo!() }", "literal").is_empty());
        assert!(candidates("fn f(a: [u8; 4]) -> usize { a.len() }", "literal").is_empty());

        // The element of an array *value* is an ordinary expression and must stay mutable; it is
        // only the length beside it that cannot hold a guard.
        assert!(!candidates("fn f() -> [u8; 4] { [7; 4] }", "literal").is_empty());
        assert!(candidates("type Row = [u8; 16];", "literal").is_empty());
    }

    #[test]
    fn const_generic_arguments_are_not_mutated() {
        // Same reason as an array length: the argument is a const expression, and it can sit
        // arbitrarily deep inside a type.
        assert!(candidates("struct Grid(Matrix<3>);", "literal").is_empty());
        assert!(candidates("fn f() -> Wrapper<Inner<8>> { todo!() }", "literal").is_empty());
    }

    #[test]
    fn a_value_beside_an_inert_type_is_still_mutated() {
        // Making types inert must not swallow the function they belong to; the array length is a
        // const context but the body around it is ordinary code.
        let source = "fn f(a: [u8; 4], b: i32) -> i32 { b + 1 }";

        assert!(!candidates(source, "arith").is_empty());
    }

    #[test]
    fn macro_interiors_are_not_mutated() {
        let source = "fn f() { println!(\"{}\", 1 + 2); }";

        assert!(candidates(source, "arith").is_empty());
    }

    #[test]
    fn if_conditions_can_be_negated() {
        let source = "fn f(a: bool) -> i32 { if a { 1 } else { 2 } }";
        let found = mutators(source, "cond.negate");

        assert_eq!(found, vec!["cond.negate"]);
    }

    #[test]
    fn a_negated_condition_is_parenthesized() {
        // `!` binds tighter than any binary operator, so `!a == b` is a different expression that
        // usually does not even type-check.
        let source = "fn f(a: i32, b: i32) -> i32 { if a == b { 1 } else { 2 } }";
        let found = candidates(source, "cond.negate");

        assert_eq!(found[0].replacement, "!(a == b)");
    }

    #[test]
    fn removing_a_unary_operator_leaves_the_operand() {
        let found = candidates("fn f(a: i32) -> i32 { -a }", "unary.remove_neg");

        assert_eq!(found[0].replacement, "a");
    }

    #[test]
    fn if_let_conditions_are_left_alone() {
        let source = "fn f(a: Option<i32>) -> i32 { if let Some(x) = a { x } else { 2 } }";

        assert!(candidates(source, "cond").is_empty());
    }

    #[test]
    fn while_conditions_can_be_negated() {
        let source = "fn f(mut a: i32) { while a > 0 { a -= 1; } }";
        let found = mutators(source, "cond.negate");

        assert_eq!(found, vec!["cond.negate"]);
    }

    #[test]
    fn integer_literals_yield_boundary_replacements() {
        let found = mutators("fn f() -> i32 { 5 }", "literal");

        assert!(found.contains(&"literal.int_to_zero"));
        assert!(found.contains(&"literal.int_to_one"));
        assert!(found.contains(&"literal.int_increment"));
        assert!(found.contains(&"literal.int_decrement"));
    }

    #[test]
    fn a_literal_zero_is_not_replaced_by_zero() {
        let found = mutators("fn f() -> i32 { 0 }", "literal");

        assert!(!found.contains(&"literal.int_to_zero"));
        assert!(found.contains(&"literal.int_to_one"));
    }

    #[test]
    fn a_literal_one_is_not_replaced_by_one() {
        let found = mutators("fn f() -> i32 { 1 }", "literal");

        assert!(!found.contains(&"literal.int_to_one"));
    }

    #[test]
    fn increment_replacements_are_the_neighbouring_values() {
        let found = candidates("fn f() -> i32 { 5 }", "literal.int_increment,literal.int_decrement");
        let mut replacements: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

        replacements.sort_unstable();
        assert_eq!(replacements, vec!["4", "6"]);
    }

    #[test]
    fn a_borrowed_literal_array_is_left_alone_so_it_can_still_be_promoted() {
        let source = "fn f(k: &str) -> Option<&'static [&'static str]> { Some(match k { \"a\" => &[\"id\", \"name\"], _ => return None }) }";
        let found = mutators(source, "literal");

        assert!(found.is_empty(), "promotable borrow was instrumented: {found:?}");
    }

    #[test]
    fn a_borrowed_array_of_computed_values_is_still_mutated() {
        let found = mutators("fn f(n: i32) -> i32 { let v = &[n + 1, n * 2]; v[0] }", "arith");

        assert!(found.contains(&"arith.add_to_sub"));
    }

    #[test]
    fn a_let_chain_condition_is_not_negated_or_replaced() {
        let source = "fn f(x: Option<i32>, y: bool) -> i32 { if let Some(n) = x && y { n } else { 0 } }";
        let found = mutators(source, "cond,logical");

        assert!(found.is_empty(), "let-chain condition was mutated: {found:?}");
    }

    #[test]
    fn a_binding_at_the_end_of_a_let_chain_is_also_recognized() {
        let source = "fn f(x: Option<i32>, y: bool) -> i32 { if y && let Some(n) = x { n } else { 0 } }";
        let found = mutators(source, "cond,logical");

        assert!(found.is_empty(), "trailing let-chain binding was mutated: {found:?}");
    }

    #[test]
    fn a_while_let_chain_condition_is_not_negated() {
        let source = "fn f(mut x: Option<i32>, y: bool) -> i32 { while let Some(_n) = x && y { x = None; } 0 }";
        let found = mutators(source, "cond");

        assert!(!found.contains(&"cond.negate"));
    }

    #[test]
    fn an_ordinary_compound_condition_is_still_mutated() {
        let found = mutators("fn f(a: bool, b: bool) -> bool { if a && b { true } else { false } }", "cond,logical");

        assert!(found.contains(&"cond.negate"));
        assert!(found.contains(&"logical.and_to_or"));
    }

    #[test]
    fn an_empty_string_literal_is_not_replaced_by_an_empty_string() {
        let found = mutators("fn f() -> &'static str { \"\" }", "literal");

        assert!(!found.contains(&"literal.str_to_empty"));
        assert!(found.contains(&"literal.str_to_xyzzy"));
    }

    #[test]
    fn the_marker_string_is_not_replaced_by_itself() {
        // The mutant would be the original program, so it could never be killed and would be
        // reported as a survivor on every run.
        let found = mutators("fn f() -> &'static str { \"xyzzy\" }", "literal");

        assert!(!found.contains(&"literal.str_to_xyzzy"), "{found:?}");
        assert!(found.contains(&"literal.str_to_empty"), "{found:?}");
    }

    #[test]
    fn a_condition_that_is_already_a_literal_is_not_replaced_by_that_literal() {
        let found = mutators("fn f() -> i32 { if true { 1 } else { 2 } }", "cond");

        assert!(!found.contains(&"cond.always_true"), "{found:?}");
        assert!(found.contains(&"cond.always_false"), "{found:?}");

        let found = mutators("fn f() -> i32 { if false { 1 } else { 2 } }", "cond");

        assert!(found.contains(&"cond.always_true"), "{found:?}");
        assert!(!found.contains(&"cond.always_false"), "{found:?}");
    }

    #[test]
    fn negated_zero_has_no_mutant() {
        // Zero is its own negation, so removing the `-` leaves the program unchanged.
        assert!(candidates("fn f() -> i32 { -0 }", "unary.remove_neg").is_empty());
        assert!(candidates("fn f() -> f64 { -0.0 }", "unary.remove_neg").is_empty());
        assert!(!candidates("fn f() -> i32 { -1 }", "unary.remove_neg").is_empty());
    }

    #[test]
    fn associated_const_initializers_are_not_mutated() {
        // A guard cannot be called in a const-evaluation context, so the mutant would not compile.
        let source = "struct S; impl S { const N: i32 = 1 + 2; }";

        assert!(candidates(source, "").is_empty(), "{:?}", candidates(source, ""));
    }

    #[test]
    fn trait_const_defaults_are_not_mutated() {
        let source = "trait T { const N: i32 = 1 + 2; }";

        assert!(candidates(source, "").is_empty(), "{:?}", candidates(source, ""));
    }

    #[test]
    fn a_string_returning_function_gets_an_owned_marker() {
        // `"xyzzy"` is a `&'static str`, so a `String`-returning function needs the owned form or
        // every one of these mutants is withdrawn as unviable.
        let found = candidates("fn f() -> String { String::new() }", "fn_value.xyzzy_string");

        assert_eq!(found[0].replacement, "\"xyzzy\".to_owned()");
    }

    #[test]
    fn booleans_flip_to_the_other_value() {
        let found = candidates("fn f() -> bool { true }", "literal.bool_flip");

        assert_eq!(found[0].replacement, "false");
    }

    #[test]
    fn compound_assignment_is_mutated() {
        let found = mutators("fn f(a: &mut i32) { *a += 1; }", "assign");

        assert_eq!(found, vec!["assign.add_to_sub"]);
    }

    #[test]
    fn logical_operators_are_mutated() {
        let found = mutators("fn f(a: bool, b: bool) -> bool { a && b }", "logical");

        assert_eq!(found, vec!["logical.and_to_or"]);
    }

    #[test]
    fn the_remaining_binary_operators_are_mutated() {
        let source = "fn f(a: i32, b: i32, x: bool, y: bool) {
            let _ = a <= b; let _ = a >= b; let _ = a != b;
            let _ = a / b; let _ = a % b;
            let _ = a & b; let _ = a | b; let _ = a ^ b;
            let _ = a << b; let _ = a >> b;
            let _ = x || y;
        }";
        let found = mutators(source, "relational,arith,bitwise,shift,logical");

        for expected in [
            "relational.le_to_lt",
            "relational.ge_to_gt",
            "relational.ne_to_eq",
            "arith.div_to_mul",
            "arith.rem_to_div",
            "bitwise.and_to_or",
            "bitwise.or_to_and",
            "bitwise.xor_to_and",
            "shift.shl_to_shr",
            "shift.shr_to_shl",
            "logical.or_to_and",
        ] {
            assert!(found.contains(&expected), "{expected} not in {found:?}");
        }
    }

    #[test]
    fn every_compound_assignment_operator_is_mutated() {
        let source = "fn f(a: &mut i32, b: i32) {
            *a -= b; *a *= b; *a /= b; *a %= b;
            *a &= b; *a |= b; *a ^= b; *a <<= b; *a >>= b;
        }";
        let found = mutators(source, "assign");

        for expected in [
            "assign.sub_to_add",
            "assign.mul_to_div",
            "assign.div_to_mul",
            "assign.rem_to_div",
            "assign.and_to_or",
            "assign.or_to_and",
            "assign.xor_to_and",
            "assign.shl_to_shr",
            "assign.shr_to_shl",
        ] {
            assert!(found.contains(&expected), "{expected} not in {found:?}");
        }
    }

    #[test]
    fn unary_operators_can_be_removed() {
        let found = mutators("fn f(a: i32) -> i32 { -a }", "unary");

        assert_eq!(found, vec!["unary.remove_neg"]);
    }

    #[test]
    fn logical_not_can_be_removed() {
        let found = candidates("fn f(a: bool) -> bool { !a }", "unary.remove_not");

        assert_eq!(found[0].replacement, "a");
    }

    #[test]
    fn statement_deletion_covers_calls_assignments_and_ignored_statements() {
        let source = "fn f(v: &mut Vec<i32>, mut a: i32) {
            v.push(1);
            a = 2;
            a += 3;
            a + 4;
        }";
        let found = mutators(source, "stmt");

        assert!(found.contains(&"stmt.delete_call"));
        assert_eq!(found.iter().filter(|name| **name == "stmt.delete_assign").count(), 2);
    }

    #[test]
    fn repeat_lengths_and_enum_discriminants_are_const_contexts() {
        let source = "enum E { A = 1 + 2 } fn f(n: i32) { let _ = [n + 1; 2 + 3]; }";
        let found = mutators(source, "arith");

        assert!(found.contains(&"arith.add_to_sub"), "{found:?}");
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn trait_default_methods_are_mutated_but_excluded_ones_are_not() {
        let source = "trait T {
            fn f(&self) -> i32 { 1 }
            #[cfg(test)]
            fn helper(&self) -> i32 { 2 }
        }";
        let found = candidates(source, "fn_value.zero,literal.int_to_zero");

        assert!(found.iter().any(|candidate| candidate.item_path == "f"));
        assert!(found.iter().all(|candidate| candidate.item_path != "helper"));
    }

    #[test]
    fn const_trait_defaults_are_left_inert() {
        let source = "trait T { const fn f(&self) -> i32 { 1 + 2 } }";

        assert!(candidates(source, "arith,fn_value").is_empty());
    }

    #[test]
    fn impl_paths_handle_reference_and_non_path_self_types() {
        let source = "trait T { fn f(&self) -> i32; }
            struct S;
            impl T for &S { fn f(&self) -> i32 { 1 + 1 } }
            impl T for (S,) { fn f(&self) -> i32 { 2 + 2 } }";
        let found = candidates(source, "arith.add_to_sub");
        let paths: Vec<&str> = found.iter().map(|candidate| candidate.item_path.as_str()).collect();

        assert!(paths.contains(&"S::f"), "{paths:?}");
        assert!(paths.contains(&"_::f"), "{paths:?}");
    }

    #[test]
    fn borrowed_promotable_shapes_are_classified_without_touching_unary_minus() {
        let source = "fn f() -> &'static (i32, [i32; 2], &'static i32, i32) {
            &((-1), [0; 2], &3, 4)
        }";
        let found = mutators(source, "literal,unary");

        assert!(!found.contains(&"unary.remove_neg"), "{found:?}");
    }

    #[test]
    fn function_value_replacements_cover_return_type_shapes() {
        let source = "fn unit() { work(); }
            fn explicit_unit() -> () { work(); }
            fn unsigned() -> usize { 3 }
            fn float() -> f64 { 3.0 }
            fn owned_string() -> String { String::new() }
            fn vec_deque() -> std::collections::VecDeque<i32> { std::collections::VecDeque::new() }
            fn reference(x: &i32) -> &i32 { x }
            fn array() -> [i32; 1] { [1] }
            fn unknown() -> Custom { make() }";
        let found = mutators(source, "fn_value");

        for expected in [
            "fn_value.unit",
            "fn_value.zero",
            "fn_value.one",
            "fn_value.minus_one",
            "fn_value.empty_string",
            "fn_value.xyzzy_string",
            "fn_value.empty_collection",
            "fn_value.one_element",
            "fn_value.default",
        ] {
            assert!(found.contains(&expected), "{expected} not in {found:?}");
        }
    }

    #[test]
    fn unselected_function_values_are_filtered_at_emit_time() {
        let found = candidates("fn f() -> i32 { 1 }", "fn_value.one");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mutator, "fn_value.one");
    }

    #[test]
    fn literals_without_literal_mutators_are_ignored() {
        let found = candidates("fn f() -> char { 'x' }", "literal");

        assert!(found.is_empty());
    }

    #[test]
    fn huge_integer_literals_skip_neighbour_replacements() {
        let found = mutators("fn f() -> u128 { 340282366920938463463374607431768211455 }", "literal");

        assert!(found.contains(&"literal.int_to_zero"));
        assert!(!found.contains(&"literal.int_increment"));
        assert!(!found.contains(&"literal.int_decrement"));
    }

    #[test]
    fn ids_are_stable_across_reformatting() {
        let compact = "fn f(a: i32, b: i32) -> bool { a < b }";
        let spaced = "fn f(a: i32, b: i32) -> bool {\n\n    a  <  b\n\n}\n";

        let left = SourceFile::parse("test.rs", compact.to_owned()).unwrap();
        let right = SourceFile::parse("test.rs", spaced.to_owned()).unwrap();
        let selection = Selection::parse("relational").unwrap();

        let left_ids: Vec<String> = into_mutants(&left, "p", collect(&left, &selection))
            .into_iter()
            .map(|m| m.id)
            .collect();

        let right_ids: Vec<String> = into_mutants(&right, "p", collect(&right, &selection))
            .into_iter()
            .map(|m| m.id)
            .collect();

        assert_eq!(left_ids, right_ids);
    }

    #[test]
    fn ids_survive_a_line_inserted_above() {
        let before = "fn f(a: i32, b: i32) -> bool { a < b }";
        let after = "// a new comment\n\nfn f(a: i32, b: i32) -> bool { a < b }";

        let left = SourceFile::parse("test.rs", before.to_owned()).unwrap();
        let right = SourceFile::parse("test.rs", after.to_owned()).unwrap();
        let selection = Selection::parse("relational").unwrap();

        let left_ids: Vec<String> = into_mutants(&left, "p", collect(&left, &selection))
            .into_iter()
            .map(|m| m.id)
            .collect();

        let right_ids: Vec<String> = into_mutants(&right, "p", collect(&right, &selection))
            .into_iter()
            .map(|m| m.id)
            .collect();

        assert_eq!(left_ids, right_ids);
    }

    #[test]
    fn identical_sites_in_one_function_get_distinct_ids() {
        let source = "fn f(a: i32, b: i32) -> bool { (a < b) && (a < b) }";
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let selection = Selection::parse("relational.lt_to_le").unwrap();
        let mutants = into_mutants(&file, "p", collect(&file, &selection));

        assert_eq!(mutants.len(), 2);
        assert_ne!(mutants[0].id, mutants[1].id);
        assert_eq!(mutants[0].occurrence, 0);
        assert_eq!(mutants[1].occurrence, 1);
    }

    #[test]
    fn identical_sites_in_different_functions_get_distinct_ids() {
        let source = "fn f(a: i32, b: i32) -> bool { a < b }\nfn g(a: i32, b: i32) -> bool { a < b }";
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let selection = Selection::parse("relational.lt_to_le").unwrap();
        let mutants = into_mutants(&file, "p", collect(&file, &selection));

        assert_eq!(mutants.len(), 2);
        assert_ne!(mutants[0].id, mutants[1].id);
        assert_eq!(mutants[0].occurrence, 0);
        assert_eq!(mutants[1].occurrence, 0);
    }

    #[test]
    fn different_replacements_at_one_site_get_distinct_ids() {
        let source = "fn f(a: i32, b: i32) -> bool { a < b }";
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let selection = Selection::parse("relational").unwrap();
        let mutants = into_mutants(&file, "p", collect(&file, &selection));

        assert_eq!(mutants.len(), 2);
        assert_ne!(mutants[0].id, mutants[1].id);
    }

    #[test]
    fn mutants_carry_line_and_column() {
        let source = "fn f(a: i32, b: i32) -> bool {\n    a < b\n}";
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let selection = Selection::parse("relational.lt_to_le").unwrap();
        let mutants = into_mutants(&file, "p", collect(&file, &selection));

        assert_eq!(mutants[0].line, 2);
        assert_eq!(mutants[0].column, 5);
    }

    #[test]
    fn doc_comments_are_not_string_literals() {
        // A doc comment is desugared into `#[doc = "..."]`, so a visitor that walks attributes
        // reports every line of documentation in the tree as a mutable string.
        let source = "/// documentation\nfn f() {}";

        assert!(candidates(source, "literal").is_empty());
    }

    #[test]
    fn attribute_arguments_are_not_mutated() {
        let source = "#[deprecated(note = \"use g instead\", since = \"1.0\")]\nfn f() {}";

        assert!(candidates(source, "all").is_empty());
    }

    #[test]
    fn string_literals_in_real_code_are_still_mutated() {
        let source = "/// documentation\nfn f() -> &'static str { \"hello\" }";
        let found = mutators(source, "literal.str_to_empty");

        assert_eq!(found, vec!["literal.str_to_empty"]);
    }

    #[test]
    fn an_empty_file_yields_nothing() {
        assert!(candidates("", "all").is_empty());
    }

    #[test]
    fn a_file_of_only_types_yields_nothing() {
        assert!(candidates("struct S { a: i32 } enum E { A, B }", "all").is_empty());
    }

    // ---- Match guards. -----------------------------------------------------------------------

    #[test]
    fn a_match_guard_is_mutated_the_way_a_branch_condition_is() {
        let source = "fn f(v: i32) -> i32 { match v { n if n > 0 => n, _ => 0 } }";
        let found = mutators(source, "match_guard");

        // Before this family a guard was the one condition in the language nothing asked about,
        // so a suite that never exercised the guarded case scored as though it had.
        assert!(found.contains(&"match_guard.negate"), "{found:?}");
        assert!(found.contains(&"match_guard.always_true"), "{found:?}");
        assert!(found.contains(&"match_guard.always_false"), "{found:?}");
    }

    #[test]
    fn an_unguarded_arm_offers_no_guard_mutants() {
        let source = "fn f(v: i32) -> i32 { match v { 1 => 1, _ => 0 } }";

        assert!(candidates(source, "match_guard").is_empty());
    }

    #[test]
    fn a_guard_that_is_already_a_literal_is_not_replaced_by_that_literal() {
        let source = "fn f(v: i32) -> i32 { match v { n if true => n, _ => 0 } }";
        let found = mutators(source, "match_guard");

        // Replacing `true` with `true` is the original program, which can never be caught and
        // would sit in the report as a permanent survivor.
        assert!(!found.contains(&"match_guard.always_true"), "{found:?}");
        assert!(found.contains(&"match_guard.always_false"), "{found:?}");
    }

    // ---- Match arms. -------------------------------------------------------------------------

    #[test]
    fn an_arm_before_a_wildcard_can_be_stopped_from_matching() {
        let source = "fn f(v: i32) -> i32 { match v { 1 => 10, 2 => 20, _ => 0 } }";
        let found = candidates(source, "match_arm");

        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|c| c.shape == Shape::Arm), "{found:?}");
    }

    #[test]
    fn the_wildcard_itself_is_never_stopped_from_matching() {
        let source = "fn f(v: i32) -> i32 { match v { 1 => 10, _ => 0 } }";
        let found = candidates(source, "match_arm");

        // Guarding the wildcard leaves the match non-exhaustive, which is a compile error rather
        // than a question about the tests: the compiler does not count a guarded arm.
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].span, span_of(source, "1 => 10").start..span_of(source, "1 => 10").start + 1);
    }

    #[test]
    fn a_match_without_a_wildcard_offers_no_arm_mutants() {
        let source = "fn f(v: bool) -> i32 { match v { true => 1, false => 0 } }";

        assert!(candidates(source, "match_arm").is_empty(), "an exhaustive match has nothing to fall through to");
    }

    #[test]
    fn an_arm_after_the_wildcard_offers_nothing() {
        let source = "fn f(v: i32) -> i32 { match v { _ => 0, 1 => 10 } }";

        // Nothing falls through to an arm the wildcard already swallowed, so the mutant would be
        // an equivalent one that survives forever.
        assert!(candidates(source, "match_arm").is_empty());
    }

    #[test]
    fn a_guarded_arm_is_disabled_by_its_guard_rather_than_by_a_second_mutant() {
        let source = "fn f(v: i32) -> i32 { match v { n if n > 0 => n, _ => 0 } }";

        // `match_guard.always_false` already stops the arm matching. A second mutant saying the
        // same thing would double the cost of one question.
        assert!(candidates(source, "match_arm").is_empty());
    }

    // ---- Struct literal fields. --------------------------------------------------------------

    #[test]
    fn a_struct_field_is_omitted_only_when_a_base_supplies_it() {
        let source = "fn f() -> C { C { a: 1, b: 2, ..Default::default() } }";
        let found = candidates(source, "struct_field");

        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|c| c.replacement.contains("b: 2") && !c.replacement.contains("a: 1")));
        assert!(found.iter().any(|c| c.replacement.contains("a: 1") && !c.replacement.contains("b: 2")));
    }

    #[test]
    fn a_struct_literal_without_a_base_offers_nothing() {
        let source = "fn f() -> C { C { a: 1, b: 2 } }";

        // Removing a field from a literal that names every one of them does not compile.
        assert!(candidates(source, "struct_field").is_empty());
    }

    #[test]
    fn omitting_the_last_field_leaves_the_base_intact() {
        let source = "fn f() -> C { C { a: 1, ..Default::default() } }";
        let found = candidates(source, "struct_field");

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].replacement, "C { ..Default::default() }");
    }

    // ---- Ranges. -----------------------------------------------------------------------------

    #[test]
    fn a_half_open_range_offers_its_inclusive_form() {
        let source = "fn f(n: usize) -> usize { let mut t = 0; for i in 0..n { t += i; } t }";
        let found = candidates(source, "range");

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].mutator, "range.exclusive_to_inclusive");

        // Spelled as arithmetic on the endpoint rather than as `..=`, because the mutant and the
        // original share the arms of an `if` and so have to share a type.
        assert_eq!(found[0].replacement, "(0)..((n) + 1)");
    }

    #[test]
    fn an_inclusive_range_offers_its_half_open_form() {
        let source = "fn f(n: usize) -> usize { let mut t = 0; for i in 0..=n { t += i; } t }";
        let found = candidates(source, "range");

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].mutator, "range.inclusive_to_exclusive");
        assert_eq!(found[0].replacement, "(0)..=((n) - 1)");
    }

    #[test]
    fn a_range_with_no_end_has_no_inclusive_form_to_offer() {
        let source = "fn f(v: &[u8]) -> &[u8] { &v[1..] }";

        assert!(candidates(source, "range").is_empty());
    }

    #[test]
    fn a_range_with_no_start_still_moves_its_boundary() {
        let source = "fn f(v: &[u8], n: usize) -> &[u8] { &v[..n] }";
        let found = candidates(source, "range");

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].replacement, "..((n) + 1)");
    }

    // ---- Loop exits. -------------------------------------------------------------------------

    #[test]
    fn break_and_continue_are_swapped_for_each_other() {
        let source = "fn f(v: &[i32]) { for x in v { if *x == 0 { continue; } if *x == 1 { break; } } }";
        let found = mutators(source, "loop.break_to_continue,loop.continue_to_break");

        assert!(found.contains(&"loop.break_to_continue"), "{found:?}");
        assert!(found.contains(&"loop.continue_to_break"), "{found:?}");
    }

    #[test]
    fn a_break_carrying_a_value_is_left_alone() {
        let source = "fn f() -> i32 { loop { break 1; } }";

        // `continue` produces no value, so the loop would no longer have the type its context
        // requires and the mutant would be withdrawn as unviable rather than measured.
        assert!(!mutators(source, "loop.break_to_continue").contains(&"loop.break_to_continue"));
    }

    #[test]
    fn a_labelled_break_is_left_alone_but_a_labelled_continue_is_not() {
        let source = "fn f(v: &[i32]) { 'outer: for x in v { for y in v { if x == y { continue 'outer; } break 'outer; } } }";
        let found = candidates(source, "loop");

        // A label on `continue` can only name a loop, so `break` accepts it. A label on `break`
        // may name a labelled block, which `continue` cannot leave at all.
        assert!(found.iter().any(|c| c.mutator == "loop.continue_to_break" && c.replacement == "break 'outer"));
        assert!(!found.iter().any(|c| c.mutator == "loop.break_to_continue"));
    }

    #[test]
    fn a_break_or_continue_statement_can_be_deleted() {
        let source = "fn f(v: &[i32]) { for x in v { if *x == 0 { continue; } } }";
        let found = candidates(source, "loop.delete_continue");

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].shape, Shape::Stmt);
    }

    // ---- Focused numeric perturbation. --------------------------------------------------------

    #[test]
    fn a_call_argument_is_perturbed_by_one_in_both_directions() {
        let source = "fn f(n: usize) { g(n); }";
        let found = candidates(source, "expr");

        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|c| c.replacement == "(n) + 1"));
        assert!(found.iter().any(|c| c.replacement == "(n) - 1"));
    }

    #[test]
    fn a_literal_argument_is_left_to_the_literal_family() {
        let source = "fn f() { g(3); }";

        // `literal.int_increment` already offers `4` here. Offering `(3) + 1` beside it would buy
        // a second run of the whole suite for an answer already in hand.
        assert!(candidates(source, "expr").is_empty());
    }

    #[test]
    fn a_capacity_argument_is_not_perturbed() {
        let source = "fn f(n: usize) -> Vec<u8> { Vec::with_capacity(n) }";

        // A test that noticed would be a test pinning an allocation strategy, so reporting the
        // survivor would accuse the suite of a gap it should not be asked to fill.
        assert!(candidates(source, "expr").is_empty());
    }

    #[test]
    fn an_index_and_a_range_bound_are_perturbed() {
        let indexed = candidates("fn f(v: &[u8], i: usize) -> u8 { v[i] }", "expr");
        let bounded = candidates("fn f(v: &[u8], n: usize) -> &[u8] { &v[..n] }", "expr");

        // Two positions overlap here and both are wanted: the subscript, where being wrong by one
        // reads the neighbouring element, and the returned `u8`, where being wrong by one is the
        // classic off-by-one in the answer itself.
        assert!(indexed.iter().any(|c| c.replacement == "(i) + 1"), "{indexed:?}");
        assert!(indexed.iter().any(|c| c.replacement == "(v[i]) + 1"), "{indexed:?}");

        // The returned `&[u8]` is not a number, so only the range bound is offered.
        assert_eq!(bounded.len(), 2, "{bounded:?}");
        assert!(bounded.iter().any(|c| c.replacement == "(n) - 1"), "{bounded:?}");
    }

    #[test]
    fn a_returned_value_is_perturbed_however_it_is_returned() {
        let trailing = candidates("fn f(n: usize) -> usize { n }", "expr");
        let explicit = candidates("fn f(n: usize) -> usize { return n; }", "expr");

        assert_eq!(trailing.len(), 2, "{trailing:?}");
        assert_eq!(explicit.len(), 2, "{explicit:?}");
    }

    #[test]
    fn a_non_numeric_argument_is_not_perturbed() {
        let source = "fn f(s: &str, c: bool, n: usize) { g(s, c, &s, |x| x, \"lit\", n); }";
        let found = candidates(source, "expr");

        // Nothing here adds to an integer except `n`. Every mutant that cannot compile costs a
        // share of a rebuild that finds nothing, so the filter is what keeps the family
        // affordable — but the one argument that does add has to survive it, or the filter has
        // bought its saving by hiding a gap in the suite.
        assert!(found.iter().all(|c| c.replacement.starts_with("(n)")), "{found:?}");
        assert!(found.iter().any(|c| c.replacement == "(n) + 1"), "{found:?}");
    }

    #[test]
    fn a_parameter_the_source_declares_a_number_is_still_perturbed_through_a_reference() {
        // `&usize + 1` compiles, so treating every reference as unaddable would throw away a
        // mutant that builds, runs and can genuinely be missed.
        let found = candidates("fn f(n: &usize) { g(n); }", "expr");

        assert!(found.iter().any(|c| c.replacement == "(n) + 1"), "{found:?}");
    }

    #[test]
    fn an_annotated_local_is_judged_by_the_type_the_source_wrote_down() {
        let source = "fn f() { let name: String = h(); let count: u32 = h(); g(name); g(count); }";
        let found = candidates(source, "expr");

        assert!(found.iter().any(|c| c.replacement == "(count) + 1"), "{found:?}");
        assert!(!found.iter().any(|c| c.replacement.starts_with("(name)")), "{found:?}");
    }

    #[test]
    fn a_local_whose_type_was_never_written_down_is_left_alone_rather_than_guessed_at() {
        // The two mistakes do not cost the same. An unviable mutant costs a share of one rebuild;
        // a viable mutant dropped on a guess is a hole in the report nothing else would reveal.
        let found = candidates("fn f() { let total = h(); g(total); }", "expr");

        assert!(found.iter().any(|c| c.replacement == "(total) + 1"), "{found:?}");
    }

    #[test]
    fn a_type_named_where_a_value_is_expected_is_not_perturbed() {
        let source = "fn f() { g(PhantomData, Vec::new(), items.iter(), MAX); }";
        let found = candidates(source, "expr");

        // `MAX` is the point of the exception: constants are spelled in the screaming case and are
        // among the most worthwhile things this family has to offer, so the camel-case rule that
        // rejects `PhantomData` must not reject them too.
        assert!(found.iter().any(|c| c.replacement == "(MAX) + 1"), "{found:?}");
        assert!(found.iter().all(|c| c.replacement.starts_with("(MAX)")), "{found:?}");
    }

    #[test]
    fn a_local_binding_does_not_leak_into_a_nested_function() {
        // A function defined inside another cannot see the outer one's locals, so reasoning from
        // them would reach a confident conclusion about a completely unrelated name.
        let source = "fn outer() { let value: String = h(); fn inner(value: u32) { g(value); } }";
        let found = candidates(source, "expr");

        assert!(found.iter().any(|c| c.replacement == "(value) + 1"), "{found:?}");
    }

    #[test]
    fn a_default_is_not_invented_for_a_type_the_caller_chooses() {
        // `D::Error` is whatever the caller's deserializer says it is, and nothing promises it has
        // a `Default`. On a serde-shaped API this was the single largest source of mutants that
        // could not compile.
        let source = "fn f<D: Reader>(d: D) -> Result<usize, D::Error> { g(d); Ok(1) }";
        let found = candidates(source, "fn_value");

        assert!(!found.iter().any(|c| c.replacement.contains("Err(Default::default())")), "{found:?}");

        // The other half of the return type is concrete, so it keeps everything it had. A rule
        // that took the whole signature out would stop asking whether the value is tested at all.
        assert!(found.iter().any(|c| c.replacement == "Ok(0)"), "{found:?}");
    }

    #[test]
    fn a_default_is_still_invented_for_a_parameter_declared_to_have_one() {
        // The promise this rule looks for was made explicitly, so the mutant it would otherwise
        // withhold compiles and is worth offering.
        let source = "fn f<T: Default>(t: T) -> Result<usize, T> { g(t); Ok(1) }";
        let found = candidates(source, "fn_value");

        assert!(found.iter().any(|c| c.replacement == "Err(Default::default())"), "{found:?}");
    }

    #[test]
    fn a_default_is_not_invented_for_a_trait_object() {
        // `dyn Reader` names a capability rather than a type; there is no `default()` to call.
        let found = candidates("fn f() -> Box<dyn Reader> { h() }", "fn_value");

        assert!(!found.iter().any(|c| c.replacement.contains("Default::default()")), "{found:?}");
    }

    #[test]
    fn an_associated_type_of_self_is_still_given_a_default() {
        // `Self::Value` looks like `D::Error` but is not: inside an `impl` it resolves to a type
        // that block chose, which often does have a `Default`. Treating it as abstract cost six
        // mutants a real suite had caught.
        let source = "impl Visitor for V { fn visit(self) -> Result<Self::Value, u8> { h() } }";
        let found = candidates(source, "fn_value");

        assert!(found.iter().any(|c| c.replacement == "Ok(Default::default())"), "{found:?}");
    }

    #[test]
    fn perturbation_is_on_by_default() {
        let source = "fn f(n: usize) { g(n); }";
        let found = mutators(source, "@default");

        assert!(found.contains(&"expr.increment"), "{found:?}");
    }

    #[test]
    fn option_and_result_construction_is_mutated_both_ways() {
        let found = mutators("fn f(flag: bool) { let _ = if flag { Some(1) } else { None }; }", "option");

        assert!(found.contains(&"option.some_to_none"), "{found:?}");
        assert!(found.contains(&"option.none_to_some"), "{found:?}");

        let found = mutators("fn f(flag: bool) { let _ = if flag { Ok(1) } else { Err(2) }; }", "result");

        assert!(found.contains(&"result.ok_to_err"), "{found:?}");
        assert!(found.contains(&"result.err_to_ok"), "{found:?}");
    }

    #[test]
    fn iterator_methods_swap_only_where_the_types_agree() {
        let found = mutators("fn f(v: &[u32]) { let _ = v.iter().any(|n| *n > 0); }", "iter");

        assert!(found.contains(&"iter.any_to_all"), "{found:?}");

        // `take` and `skip` return different types, so no mutant may be offered for them. This
        // would otherwise be generated on every chain in a codebase and withdrawn on every run.
        let found = mutators("fn f(v: &[u32], n: usize) { let _ = v.iter().take(n).count(); }", "iter");

        assert!(!found.contains(&"iter.take_to_skip"), "{found:?}");
    }

    #[test]
    fn a_method_rename_needs_the_arity_that_identifies_it() {
        // Without type resolution, the count is the only evidence that this `take` belongs to
        // `Iterator` rather than to `Option` or `Cell`, where the rename would be nonsense.
        let found = mutators("fn f(v: &[String], s: &str) { let _ = v.iter().any(|w| w.starts_with(s)); }", "string");

        assert!(found.contains(&"string.starts_with_to_ends_with"), "{found:?}");

        let found = mutators("fn f(o: &mut Option<u32>) { let _ = o.take(); }", "iter,string");

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_vec_literal_offers_each_element_for_omission() {
        let found = candidates("fn f() { let _ = vec![1, 2, 3]; }", "collection.omit_element");

        assert_eq!(found.len(), 3);

        // The replacement must still be an expression, because it becomes one arm of an `if`.
        assert!(found.iter().all(|candidate| candidate.replacement.starts_with("vec!")), "{found:?}");
    }

    #[test]
    fn an_assignment_offers_a_default_value() {
        let found = mutators("fn f(mut n: u32) { n = n + 1; }", "assign_value");

        assert!(found.contains(&"assign_value.default"), "{found:?}");
    }

    #[test]
    fn nested_return_types_recurse_into_their_payloads() {
        let source = "fn f() -> Result<Option<bool>, String> { Ok(Some(true)) }";
        let found = candidates(source, "fn_value");
        let texts: Vec<_> = found.iter().map(|candidate| candidate.replacement.as_str()).collect();

        assert!(texts.contains(&"Ok(None)"), "{texts:?}");
        assert!(texts.contains(&"Ok(Some(true))"), "{texts:?}");
        assert!(texts.contains(&"Ok(Some(false))"), "{texts:?}");
    }

    #[test]
    fn an_impl_iterator_return_offers_nothing() {
        // An `impl Trait` return is one concrete type picked by the body, and a mutant shares an
        // `if` with that body, so every replacement would be withdrawn after a wasted build.
        let found = mutators("fn f() -> impl Iterator<Item = u32> { core::iter::once(1) }", "fn_value");

        assert!(found.is_empty(), "{found:?}");
    }

    fn span_of(text: &str, needle: &str) -> core::ops::Range<usize> {
        let start = text.find(needle).expect("the needle must be in the text");

        start..start + needle.len()
    }
}
