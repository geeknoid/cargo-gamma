use core::mem;
use core::ops::Range;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

use crate::Result;
use crate::error::Error;
use crate::model::Mutant;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

use super::intent::Intent;
use crate::model::Channel;

/// One directive, resolved and located.
#[derive(Debug, Clone)]
pub struct Directive {
    /// What the directive asks for.
    pub intent: Intent,

    /// The mutators it names, already resolved against the registry.
    pub selection: Selection,

    /// The selector text as written, for diagnostics.
    pub selectors: String,

    /// The stated reason, if any.
    pub reason: Option<String>,

    /// The stated tag, if any.
    pub tag: Option<String>,

    /// How the directive arrived.
    pub channel: Channel,

    /// One-based line the directive appears on.
    pub line: usize,

    /// The byte range the directive governs.
    pub scope: Range<usize>,
}

impl Directive {
    /// Returns whether this directive governs a mutant.
    #[must_use]
    pub fn governs(&self, mutant: &Mutant) -> bool {
        mutant.span.start >= self.scope.start
            && mutant.span.start < self.scope.end
            && self.selection.contains(&mutant.mutator)
    }
}

/// Turns parsed arguments into a directive.
pub(super) fn build(
    intent: Intent,
    arguments: &TokenStream,
    channel: Channel,
    line: usize,
    scope: Range<usize>,
    file: &SourceFile,
) -> Result<Directive> {
    let parsed = parse_arguments(arguments);
    let selection = if parsed.selectors.is_empty() {
        // A bare directive means all of them, which is what both `#[gamma::skip]` and
        // `#[mutants::skip]` have always meant.
        Selection::everything()
    } else {
        let mut selection = Selection::empty();

        selection
            .apply(&parsed.selectors)
            .map_err(|error| Error::new(format!("{}:{line}: {error}", file.path)).usage())?;

        selection
    };

    Ok(Directive {
        intent,
        selection,
        selectors: parsed.selectors,
        reason: parsed.reason,
        tag: parsed.tag,
        channel,
        line,
        scope,
    })
}

/// The parts of a directive's argument list.
#[derive(Debug, Default)]
struct Arguments {
    selectors: String,
    reason: Option<String>,
    tag: Option<String>,
}

/// Splits a directive's arguments into selectors and named values.
///
/// The tokens are read directly rather than through `syn`'s meta parser, because a selector like
/// `arith.add_to_sub` or `@default` or `!bitwise` is a perfectly good token sequence but not a
/// well-formed meta path. Reading tokens keeps the directive grammar identical to the one
/// `--ops` accepts, which is the whole point of having a single vocabulary.
fn parse_arguments(tokens: &TokenStream) -> Arguments {
    let mut arguments = Arguments::default();
    let mut selectors: Vec<String> = Vec::new();
    let mut current: Vec<TokenTree> = Vec::new();

    let mut flush = |current: &mut Vec<TokenTree>, arguments: &mut Arguments| {
        if current.is_empty() {
            return;
        }

        let taken = mem::take(current);

        // `name = "value"` is a named argument; anything else is a selector.
        if let [TokenTree::Ident(name), TokenTree::Punct(equals), TokenTree::Literal(value)] = taken.as_slice()
            && equals.as_char() == '='
        {
                let text = unquote(&value.to_string());

                match name.to_string().as_str() {
                    "reason" => arguments.reason = Some(text),
                    "tag" => arguments.tag = Some(text),
                    _ => {}
                }

            return;
        }

        let rendered: String = taken
            .iter()
            .map(|token| match token {
                TokenTree::Literal(literal) => unquote(&literal.to_string()),
                other => other.to_string(),
            })
            .collect::<String>();

        let cleaned: String = rendered.chars().filter(|character| !character.is_whitespace()).collect();

        if !cleaned.is_empty() {
            selectors.push(cleaned);
        }
    };

    for token in tokens.clone() {
        match &token {
            TokenTree::Punct(punct) if punct.as_char() == ',' => flush(&mut current, &mut arguments),
            TokenTree::Group(group) if group.delimiter() == Delimiter::None => {
                current.extend(group.stream());
            }
            _ => current.push(token),
        }
    }

    flush(&mut current, &mut arguments);
    arguments.selectors = selectors.join(",");
    arguments
}

/// Strips the quotes from a string literal's rendered form.
fn unquote(text: &str) -> String {
    text.trim_matches('"').to_owned()
}

#[cfg(test)]
mod tests {
    use super::super::directives;
    use super::*;
    use crate::parse::SourceFile;
    use proc_macro2::{Delimiter, Group, Ident, Literal, Punct, Spacing, TokenStream, TokenTree};

    fn file(source: &str) -> SourceFile {
        SourceFile::parse("test.rs", source.to_owned()).unwrap()
    }

    #[test]
    fn a_dotted_selector_survives_being_read_as_tokens() {
        let source = "#[gamma::skip(arith.add_to_sub)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].selectors, "arith.add_to_sub");
    }

    #[test]
    fn a_profile_selector_is_accepted() {
        let source = "#[gamma::skip(@arithmetic)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].selectors, "@arithmetic");
        assert!(found[0].selection.contains("arith.add_to_sub"));
    }

    #[test]
    fn a_negated_selector_is_accepted() {
        let source = "#[gamma::skip(arith, !arith.add_to_sub)]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert!(!found[0].selection.contains("arith.add_to_sub"));
        assert!(found[0].selection.contains("arith.mul_to_div"));
    }

    #[test]
    fn a_reason_is_captured() {
        let source = "#[gamma::skip(arith, reason = \"fixed point\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].reason.as_deref(), Some("fixed point"));
        assert_eq!(found[0].selectors, "arith");
    }

    #[test]
    fn a_tag_is_captured() {
        let source = "#[gamma::skip(arith, tag = \"perf\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].tag.as_deref(), Some("perf"));
    }

    #[test]
    fn unknown_named_arguments_are_ignored() {
        let source = "#[gamma::skip(arith, ticket = \"T-1\")]\nfn f(a: i32) -> i32 { a + 1 }";
        let found = directives(&file(source)).unwrap();

        assert_eq!(found[0].selectors, "arith");
        assert!(found[0].reason.is_none());
        assert!(found[0].tag.is_none());
    }

    #[test]
    fn literal_selectors_and_none_delimited_groups_are_rendered() {
        let literal = TokenTree::Literal(Literal::string("arith.add_to_sub"));
        let comma = TokenTree::Punct(Punct::new(',', Spacing::Alone));
        let grouped = TokenTree::Group(Group::new(
            Delimiter::None,
            TokenStream::from(TokenTree::Ident(Ident::new("literal", proc_macro2::Span::call_site()))),
        ));
        let tokens = [literal, comma, grouped].into_iter().collect();
        let arguments = parse_arguments(&tokens);

        assert_eq!(arguments.selectors, "arith.add_to_sub,literal");
    }

    #[test]
    fn an_unknown_selector_is_a_hard_error() {
        let source = "#[gamma::skip(arith.add_to_multiply)]\nfn f(a: i32) -> i32 { a + 1 }";
        let error = directives(&file(source)).unwrap_err();

        assert!(error.is_usage());
        assert!(error.to_string().contains("add_to_multiply"));
    }

    #[test]
    fn an_unknown_directive_name_is_a_hard_error() {
        let source = "// #[gamma::skipp(arith)]\nfn f(a: i32) -> i32 { a + 1 }";

        _ = directives(&file(source)).expect_err("the directive was expected to be rejected");
    }

    #[test]
    fn a_malformed_comment_directive_is_a_hard_error() {
        let source = "// #[gamma::skip(arith\nfn f(a: i32) -> i32 { a + 1 }";

        _ = directives(&file(source)).expect_err("the directive was expected to be rejected");
    }

    #[test]
    fn a_directive_governing_nothing_is_a_hard_error() {
        let source = "fn f(a: i32) -> i32 { a + 1 }\n// #[gamma::skip(arith)]\n";

        _ = directives(&file(source)).expect_err("the directive was expected to be rejected");
    }
}
