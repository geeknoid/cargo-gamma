//! Finding the directives a file carries, in each of the spellings they can be written in.

use proc_macro2::TokenStream;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::{Attribute, Meta, Token};

use crate::Result;
use crate::error::Error;
use crate::model::Channel;
use crate::parse::{CommentKind, SourceFile};

use super::directive::build;
use super::scopes::Scopes;
use super::{Directive, Intent};

/// The one directive name in this namespace that is not a suppression.
///
/// `#[gamma::value(<expr>)]` states the expression a return-value mutant substitutes. It is
/// understood where mutants are made rather than here, but it has to be named here so that the
/// check for a misspelled suppression does not report it as one.
const STATED_VALUE: &str = "value";

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

/// Collects directives written as real attributes.
fn attribute_directives(file: &SourceFile, scopes: &Scopes) -> Result<Vec<Directive>> {
    let mut found = Vec::new();

    for (attribute, item_span) in &scopes.attributes {
        let line = file.line_of(attribute.span().byte_range().start);

        for (path, arguments) in unwrap_cfg_attr(attribute) {
            let segments: Vec<String> = path.segments.iter().map(|segment| segment.ident.to_string()).collect();

            let (channel, intent) = match segments.as_slice() {
                [namespace] if namespace == "gamma" => (Channel::Attribute, None),
                [namespace, name] if namespace == "gamma" => {
                    // `#[gamma::value(...)]` shares the namespace and nothing else: it states an
                    // expression rather than selecting mutators, and it adds a mutant rather than
                    // withdrawing one. It is read where mutants are made, and validated there too, so it
                    // passes through here rather than being mistaken for a misspelled suppression.
                    if name == STATED_VALUE {
                        continue;
                    }

                    if name == "test_timeout_multiplier" || name == "timeout_multiplier" {
                        (Channel::Attribute, None)
                    } else if let Some(intent) = Intent::parse(name) {
                        (Channel::Attribute, Some(intent))
                    } else {
                        return Err(Error::new(format!(
                            "{}:{line}: unknown directive `{namespace}::{name}`, expected `skip`, `expect_survived`, `expect_killed`, `test_timeout_multiplier`, or `timeout_multiplier`",
                            file.path
                        ))
                        .usage());
                    }
                }
                _ => continue,
            };

            let directive = build(intent, &arguments, channel, line, item_span.clone(), file)?;
            if directive.intent.is_some() || directive.test_timeout_multiplier.is_some() {
                found.push(directive);
            }
        }
    }

    Ok(found)
}

/// Yields the directive-shaped attributes an attribute carries, seeing through `cfg_attr`.
///
/// `#[cfg_attr(test, gamma::skip)]` is a common spelling, and its outer path is `cfg_attr`, so a
/// collector that reads only the outer path silently ignores the directive inside — leaving the
/// mutants it was meant to silence in the report as survivors.
///
/// The predicate is deliberately *not* evaluated. Nothing here knows the active feature set or
/// target, and the failure modes are asymmetric: honouring a directive whose predicate is false
/// costs a few untested mutants, whereas ignoring one produces survivors the user believed they had
/// already dealt with. Suppression is a statement of intent about a site, and that intent does not
/// change with the build configuration.
fn unwrap_cfg_attr(attribute: &Attribute) -> Vec<(syn::Path, TokenStream)> {
    unwrap_meta(&attribute.meta)
}

/// Unwraps one attribute meta item, recursively following every nested `cfg_attr`.
fn unwrap_meta(meta: &Meta) -> Vec<(syn::Path, TokenStream)> {
    if !meta.path().is_ident("cfg_attr") {
        let arguments = match meta {
            Meta::List(list) => list.tokens.clone(),
            _ => TokenStream::new(),
        };

        return vec![(meta.path().clone(), arguments)];
    }

    let Meta::List(list) = meta else {
        return Vec::new();
    };

    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;

    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return Vec::new();
    };

    // The first element is the predicate; everything after it is an attribute to apply. Each of
    // those attributes may itself be conditional, so retain its original path and tokens while
    // unwrapping until a concrete attribute is reached.
    parts.into_iter().skip(1).flat_map(|meta| unwrap_meta(&meta)).collect()
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

        let Some(source) = directive_source(body) else {
            continue;
        };

        if crate::parse::exceeds_nesting_limit(&source) {
            return Err(Error::new(format!(
                "{}:{}: `{body}` nests too deeply to be safely parsed as a directive",
                file.path, comment.line
            ))
            .usage());
        }

        let parser = Attribute::parse_outer;
        let attributes = Parser::parse_str(parser, &source).map_err(|error| {
            Error::new(format!(
                "{}:{}: `{body}` is not a well-formed directive: {error}",
                file.path, comment.line
            ))
            .usage()
        })?;

        let mut recognized = false;

        for attribute in &attributes {
            let segments: Vec<String> = attribute.path().segments.iter().map(|segment| segment.ident.to_string()).collect();

            let (channel, intent) = match segments.as_slice() {
                [namespace] if namespace == "gamma" => (Channel::Comment, None),
                [namespace, name] if namespace == "gamma" => {
                    // The comment form exists because attributes cannot yet be written on statements and
                    // expressions. A stated value has no such problem — it goes on a function, where a real
                    // attribute is allowed — and nothing reads it out of a comment, so a comment carrying
                    // one would state a value that never reaches a mutant.
                    if name == STATED_VALUE {
                        return Err(Error::new(format!(
                            "{}:{}: `{namespace}::{name}` states the value a function returns and must be written as a real attribute on that function, not as a comment",
                            file.path, comment.line
                        ))
                        .usage());
                    }

                    if name == "test_timeout_multiplier" || name == "timeout_multiplier" {
                        (Channel::Comment, None)
                    } else if let Some(intent) = Intent::parse(name) {
                        (Channel::Comment, Some(intent))
                    } else {
                        return Err(Error::new(format!(
                            "{}:{}: unknown directive `{namespace}::{name}`, expected `skip`, `expect_survived`, `expect_killed`, `test_timeout_multiplier`, or `timeout_multiplier`",
                            file.path, comment.line
                        ))
                        .usage());
                    }
                }
                _ => continue,
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
                return Err(Error::new(format!("{}:{}: `{body}` does not apply to anything", file.path, comment.line)).usage());
            };

            let directive = build(intent, &arguments, channel, comment.line, scope, file)?;
            if directive.intent.is_some() || directive.test_timeout_multiplier.is_some() {
                found.push(directive);
                recognized = true;
            }
        }

        // A comment that opens with the namespace announces itself as a directive, so if nothing
        // in it resolved to one the intent has been lost. Saying so is the whole point: silence
        // here reads as a working suppression and returns survivors instead.
        if !recognized {
            return Err(Error::new(format!("{}:{}: `{body}` is not a recognized directive", file.path, comment.line)).usage());
        }
    }

    Ok(found)
}

/// Returns the attribute text to parse for a comment that announces itself as a directive.
///
/// The bracketed spelling and the bare one name the same thing, so both are normalized to a single
/// attribute text and handed to the same parser. Anything that does not open with the directive
/// namespace is prose and yields `None`, which keeps a comment that merely mentions gamma out of
/// the directive path entirely.
fn directive_source(body: &str) -> Option<String> {
    let bracketed = body.starts_with("#[");
    if bracketed {
        let inner = body.strip_prefix("#[").map_or(body, str::trim_start);
        if inner.starts_with("gamma::") || inner.starts_with("gamma(") || inner.starts_with("gamma]") {
            return Some(body.to_owned());
        }
        return None;
    }

    if body.starts_with("gamma::") || body.starts_with("gamma(") {
        return Some(format!("#[{body}]"));
    }

    None
}
