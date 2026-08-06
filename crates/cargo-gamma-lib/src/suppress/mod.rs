//! Surgical, in-source control over which mutants are generated.
//!
//! A mutation tool that cannot be told "not here, and here is why" gets switched off. Some
//! surviving mutants are genuinely uninteresting — a debug formatter, a fallback whose two arms
//! are observationally identical, a hot loop bound that no test should be asked to pin down — and
//! if the only way to silence them is a global flag, the useful signal goes with them.
//!
//! # Three channels, one vocabulary
//!
//! A directive can arrive as a real attribute, as a comment, or from configuration. All three name
//! mutators with the same selector language used by `--ops`, so there is exactly one thing to
//! learn.
//!
//! ```text
//! #[gamma::skip(arith, reason = "fixed-point math, covered by proptest")]
//! fn scale(a: i64, b: i64) -> i64 { a * b / 1000 }
//! ```
//!
//! # Why comments look exactly like attributes
//!
//! Attributes on statements and expressions are still unstable in Rust, so an attribute cannot be
//! placed on the one line a user actually wants to exempt. The comment form is deliberately the
//! attribute with `//` in front of it, and its body is handed to the same attribute parser so the
//! two forms cannot drift apart:
//!
//! ```text
//! // #[gamma::skip(arith)]
//! let total = base * rate + offset;
//! ```
//!
//! # Compatibility with `cargo-mutants`
//!
//! `#[mutants::skip]` is honoured, so a project that has already marked its uninteresting sites
//! keeps that work.

mod directive;
mod intent;
mod scopes;

use proc_macro2::TokenStream;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::{Attribute, Meta, Token};

use crate::Result;
use crate::error::Error;
use crate::model::{Channel, Expectation, Mutant, Outcome, Suppression};
use crate::parse::{CommentKind, SourceFile};

pub use directive::Directive;
pub use intent::Intent;

use directive::build;
use scopes::Scopes;

/// Finds every directive in a file.
///
/// # Errors
///
/// Returns an error if a directive names a selector that matches no mutator. This is deliberately
/// fatal rather than a warning: a suppression that quietly matches nothing leaves the score high
/// and gives no indication that the intent was lost.
pub fn directives(file: &SourceFile) -> Result<Vec<Directive>> {
    let scopes = Scopes::of(file);
    let mut found = attribute_directives(file, &scopes)?;

    found.extend(comment_directives(file, &scopes)?);
    found.sort_by_key(|directive| directive.scope.start);

    Ok(found)
}

/// Marks every mutant a `skip` directive governs, and returns how many were marked.
///
/// The mutants keep their identity and stay in the population so that reports can show what was
/// suppressed and why. Silently dropping them would make a directive indistinguishable from a
/// mutator that never fired.
pub fn suppress(mutants: &mut [Mutant], found: &[Directive]) -> usize {
    let mut count = 0;

    for mutant in mutants.iter_mut() {
        let Some(directive) = found
            .iter()
            .filter(|directive| directive.intent == Intent::Skip)
            .find(|directive| directive.governs(mutant))
        else {
            continue;
        };

        mutant.outcome = Outcome::Ignored;
        mutant.suppression = Some(Suppression {
            channel: directive.channel,
            reason: directive.reason.clone(),
            tag: directive.tag.clone(),
            line: Some(directive.line),
        });

        count += 1;
    }

    for mutant in mutants.iter_mut() {
        let expecting = found
            .iter()
            .filter(|directive| directive.intent != Intent::Skip)
            .find(|directive| directive.governs(mutant));

        if let Some(directive) = expecting {
            mutant.expectation = Some(Expectation {
                caught: directive.intent == Intent::ExpectCaught,
                line: directive.line,
                reason: directive.reason.clone(),
            });
        }
    }

    count
}

/// Collects directives written as real attributes.
fn attribute_directives(file: &SourceFile, scopes: &Scopes) -> Result<Vec<Directive>> {
    let mut found = Vec::new();

    for (attribute, item_span) in &scopes.attributes {
        let line = file.line_of(attribute.span().byte_range().start);

        for (path, arguments) in unwrap_cfg_attr(attribute) {
            let segments: Vec<String> = path.segments.iter().map(|segment| segment.ident.to_string()).collect();

            let [namespace, name] = segments.as_slice() else {
                continue;
            };

            let channel = match namespace.as_str() {
                "gamma" => Channel::Attribute,
                "mutants" => Channel::MutantsAttribute,
                _ => continue,
            };

            // A misspelling is the whole hazard: the attribute reads as if it works, and the
            // mutants it was meant to silence come back as survivors. The comment form already
            // rejects an unknown name, so the attribute form does too.
            //
            // `mutants` is exempt because it is another tool's namespace: it has directives beyond
            // `skip` that are meaningless here, and rejecting them would break a source tree that
            // is perfectly valid for the tool that owns them.
            let Some(intent) = Intent::parse(name) else {
                if channel == Channel::MutantsAttribute {
                    continue;
                }

                return Err(Error::new(format!(
                    "{}:{line}: unknown directive `{namespace}::{name}`, expected `skip`, `expect_missed` or `expect_caught`",
                    file.path
                ))
                .usage());
            };

            found.push(build(intent, &arguments, channel, line, item_span.clone(), file)?);
        }
    }

    Ok(found)
}

/// Yields the directive-shaped attributes an attribute carries, seeing through `cfg_attr`.
///
/// `#[cfg_attr(test, mutants::skip)]` is a common spelling, and its outer path is `cfg_attr`, so a
/// collector that reads only the outer path silently ignores the directive inside — leaving the
/// mutants it was meant to silence in the report as survivors.
///
/// The predicate is deliberately *not* evaluated. Nothing here knows the active feature set or
/// target, and the failure modes are asymmetric: honouring a directive whose predicate is false
/// costs a few untested mutants, whereas ignoring one produces survivors the user believed they had
/// already dealt with. Suppression is a statement of intent about a site, and that intent does not
/// change with the build configuration.
fn unwrap_cfg_attr(attribute: &Attribute) -> Vec<(syn::Path, TokenStream)> {
    if !attribute.path().is_ident("cfg_attr") {
        let arguments = match &attribute.meta {
            Meta::List(list) => list.tokens.clone(),
            _ => TokenStream::new(),
        };

        return vec![(attribute.path().clone(), arguments)];
    }

    let Meta::List(list) = &attribute.meta else {
        return Vec::new();
    };

    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;

    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return Vec::new();
    };

    // The first element is the predicate; everything after it is an attribute to apply.
    parts
        .into_iter()
        .skip(1)
        .map(|meta| {
            let arguments = match &meta {
                Meta::List(inner) => inner.tokens.clone(),
                _ => TokenStream::new(),
            };

            (meta.path().clone(), arguments)
        })
        .collect()
}

/// Collects directives written as comments.
fn comment_directives(file: &SourceFile, scopes: &Scopes) -> Result<Vec<Directive>> {
    let mut found = Vec::new();

    for comment in &file.comments {
        // Only `//` carries directives. A doc comment is part of the crate's published text, and
        // silently giving it a second meaning would be a trap.
        if comment.kind != CommentKind::Line {
            continue;
        }

        let body = comment.body.trim();

        if !body.starts_with("#[gamma::") && !body.starts_with("#[mutants::") {
            continue;
        }

        let parser = Attribute::parse_outer;
        let attributes = Parser::parse_str(parser, body).map_err(|error| {
            Error::new(format!(
                "{}:{}: `{body}` is not a well-formed directive: {error}",
                file.path, comment.line
            ))
            .usage()
        })?;

        for attribute in &attributes {
            let segments: Vec<String> = attribute
                .path()
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();

            let [namespace, name] = segments.as_slice() else {
                continue;
            };

            let channel = if namespace == "mutants" {
                Channel::MutantsAttribute
            } else {
                Channel::Comment
            };

            let Some(intent) = Intent::parse(name) else {
                return Err(Error::new(format!(
                    "{}:{}: unknown directive `{namespace}::{name}`, expected `skip`, `expect_missed` or `expect_caught`",
                    file.path, comment.line
                ))
                .usage());
            };

            let arguments = match &attribute.meta {
                Meta::List(list) => list.tokens.clone(),
                _ => TokenStream::new(),
            };

            let scope = if comment.trailing {
                scopes.enclosing_on_line(comment.line)
            } else {
                scopes.following(comment.span.end)
            };

            let Some(scope) = scope else {
                return Err(Error::new(format!(
                    "{}:{}: `{body}` does not apply to anything",
                    file.path, comment.line
                ))
                .usage());
            };

            found.push(build(intent, &arguments, channel, comment.line, scope, file)?);
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::collect;
    use crate::ops::registry::Selection;

    fn file(source: &str) -> SourceFile {
        SourceFile::parse("test.rs", source.to_owned()).unwrap()
    }

    fn mutants_of(source: &str, ops: &str) -> (SourceFile, Vec<Mutant>) {
        let parsed = file(source);
        let selection = Selection::parse(ops).unwrap();
        let candidates = collect::collect(&parsed, &selection);
        let mut mutants = collect::into_mutants(&parsed, "p", candidates);

        for (index, mutant) in mutants.iter_mut().enumerate() {
            mutant.ordinal = u32::try_from(index).unwrap() + 1;
        }

        (parsed, mutants)
    }

    #[test]
    fn a_file_with_no_directives_yields_none() {
        assert!(directives(&file("fn f(a: i32) -> i32 { a + 1 }")).unwrap().is_empty());
    }

    #[test]
    fn the_mutants_skip_attribute_is_honored() {
        let source = "#[mutants::skip]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();

        assert_eq!(suppress(&mut mutants, &found), mutants.len());
        assert_eq!(found[0].channel, Channel::MutantsAttribute);
    }

    #[test]
    fn a_bare_skip_covers_every_mutator() {
        let source = "#[gamma::skip]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let (parsed, mut mutants) = mutants_of(source, "all");
        let found = directives(&parsed).unwrap();
        let count = suppress(&mut mutants, &found);

        assert_eq!(count, mutants.len());
        assert!(count > 0);
    }

    #[test]
    fn a_directive_only_suppresses_the_mutators_it_names() {
        let source = "// #[gamma::skip(arith.add_to_sub)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();
        let _ = suppress(&mut mutants, &found);

        for mutant in &mutants {
            let expected = if mutant.mutator == "arith.add_to_sub" {
                Outcome::Ignored
            } else {
                Outcome::Pending
            };

            assert_eq!(mutant.outcome, expected, "{}", mutant.mutator);
        }
    }

    #[test]
    fn the_reason_reaches_the_suppressed_mutant() {
        let source = "// #[gamma::skip(arith, reason = \"why not\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();
        let _ = suppress(&mut mutants, &found);
        let suppression = mutants[0].suppression.as_ref().unwrap();

        assert_eq!(suppression.reason.as_deref(), Some("why not"));
        assert_eq!(suppression.channel, Channel::Comment);
        assert_eq!(suppression.line, Some(1));
    }

    #[test]
    fn a_doc_comment_is_never_a_directive() {
        // A doc comment is published text. Giving it a second meaning would be a trap.
        let source = "/// #[gamma::skip(arith)]\nfn f(a: i32) -> i32 { a + 1 }";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn an_unrelated_attribute_is_ignored() {
        let source = "#[inline]\n#[serde(skip)]\nfn f(a: i32) -> i32 { a + 1 }";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_gamma_attribute_is_an_error_rather_than_silence() {
        // A misspelling that is silently ignored is the worst outcome: the source reads as if the
        // site is suppressed, and the mutants come back as survivors anyway.
        let source = "#[gamma::note]\nfn f(a: i32) -> i32 { a + 1 }";
        let error = directives(&file(source)).expect_err("an unknown directive is a usage error");

        assert!(error.to_string().contains("unknown directive `gamma::note`"), "{error}");
    }

    #[test]
    fn an_unknown_mutants_attribute_is_still_ignored() {
        // `mutants` is another tool's namespace and has directives that mean nothing here, so
        // rejecting them would break a tree that is perfectly valid for the tool that owns them.
        let source = "#[mutants::note]\nfn f(a: i32) -> i32 { a + 1 }";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn a_cfg_attr_wrapped_directive_is_honoured() {
        let source = "#[cfg_attr(test, mutants::skip)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].intent, Intent::Skip);
    }

    #[test]
    fn a_cfg_attr_wrapped_gamma_directive_keeps_its_arguments() {
        let source = "#[cfg_attr(feature = \"slow\", gamma::skip(arith, reason = \"why\"))]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reason.as_deref(), Some("why"));
    }

    #[test]
    fn a_cfg_attr_wrapping_something_else_is_ignored() {
        let source = "#[cfg_attr(test, derive(Debug))]\nstruct S(i32);";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn unrelated_two_segment_attributes_are_ignored() {
        let source = "#[other::skip]\nfn f(a: i32) -> i32 { a + 1 }";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn single_segment_attributes_are_ignored() {
        let source = "#[gamma]\nfn f(a: i32) -> i32 { a + 1 }";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn an_unrelated_comment_is_ignored() {
        let source = "// just explaining things\nfn f(a: i32) -> i32 { a + 1 }";

        assert!(directives(&file(source)).unwrap().is_empty());
    }

    #[test]
    fn mutants_comment_directives_keep_the_mutants_channel() {
        let source = "// #[mutants::skip(arith)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].channel, Channel::MutantsAttribute);
    }

    #[test]
    fn comment_directives_without_argument_lists_select_everything() {
        let source = "// #[gamma::skip]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let found = directives(&file(source)).unwrap();

        assert!(found[0].selection.contains("arith.add_to_sub"));
        assert!(found[0].selectors.is_empty());
    }

    #[test]
    fn extra_attributes_in_a_directive_comment_are_ignored() {
        let source = "// #[gamma::skip(arith)] #[cfg(unix)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found.len(), 1);
        assert!(found[0].selection.contains("arith.add_to_sub"));
    }

    #[test]
    fn expectation_directives_do_not_suppress() {
        let source = "#[gamma::expect_missed(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();

        assert_eq!(found[0].intent, Intent::ExpectMissed);
        assert_eq!(suppress(&mut mutants, &found), 0);
    }

    #[test]
    fn an_expectation_is_recorded_on_every_mutant_it_governs() {
        let source = "#[gamma::expect_missed(arith, reason = \"deliberately untested\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();
        let _suppressed = suppress(&mut mutants, &found);

        let expectation = mutants[0].expectation.as_ref().expect("the directive is recorded");

        assert!(!expectation.caught);
        assert_eq!(expectation.reason.as_deref(), Some("deliberately untested"));
    }

    #[test]
    fn expect_caught_records_the_opposite_expectation() {
        let source = "#[gamma::expect_caught(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();
        let _suppressed = suppress(&mut mutants, &found);

        assert!(mutants[0].expectation.as_ref().expect("the directive is recorded").caught);
    }

    #[test]
    fn a_mutant_with_no_directive_carries_no_expectation() {
        let source = "fn f(a: i32) -> i32 { a + 1 }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).unwrap();
        let _suppressed = suppress(&mut mutants, &found);

        assert!(mutants[0].expectation.is_none());
    }

    #[test]
    fn suppressed_mutants_stay_in_the_population() {
        let source = "#[gamma::skip(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let before = mutants.len();
        let found = directives(&parsed).unwrap();
        let _ = suppress(&mut mutants, &found);

        assert_eq!(mutants.len(), before);
        assert!(mutants.iter().all(|mutant| mutant.outcome == Outcome::Ignored));
    }
}
