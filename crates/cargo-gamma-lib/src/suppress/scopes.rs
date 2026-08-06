use core::ops::Range;

use proc_macro2::Span;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ImplItemFn, ItemFn, ItemImpl, ItemMod, Stmt, TraitItemFn};

use crate::HashMap;
use crate::parse::SourceFile;

/// The spans a directive can attach to.
#[derive(Debug, Default)]
pub(super) struct Scopes {
    /// Every item, statement and impl member span, sorted by start.
    spans: Vec<Range<usize>>,

    /// Every attribute, paired with the span of what it is attached to.
    pub(super) attributes: Vec<(Attribute, Range<usize>)>,

    /// Line of the start of each span, for resolving trailing comments.
    lines: HashMap<usize, Range<usize>>,
}

impl Scopes {
    /// Collects every span a directive could govern.
    pub(super) fn of(file: &SourceFile) -> Self {
        let mut collector = ScopeCollector {
            scopes: Self::default(),
            text_len: file.text.len(),
        };

        collector.visit_file(&file.ast);

        let mut scopes = collector.scopes;

        scopes.spans.sort_by(|left, right| {
            left.start
                .cmp(&right.start)
                .then_with(|| right.end.cmp(&left.end))
        });

        for span in &scopes.spans {
            let line = file.line_of(span.start);
            let entry = scopes.lines.entry(line).or_insert_with(|| span.clone());

            // Prefer the widest span starting on a line, so a trailing comment on a one-line
            // function governs the function rather than its first sub-expression.
            if span.end > entry.end {
                *entry = span.clone();
            }
        }

        scopes
    }

    /// Returns the span a directive placed on its own line governs.
    ///
    /// It is the *outermost* construct beginning after the directive. A directive above a function
    /// governs the whole function, not merely its first statement, which is what a reader writing
    /// one above a function obviously intends.
    pub(super) fn following(&self, offset: usize) -> Option<Range<usize>> {
        let mut best: Option<Range<usize>> = None;

        for span in &self.spans {
            if span.start < offset {
                continue;
            }

            match &best {
                None => best = Some(span.clone()),
                Some(current) if span.start < current.start => best = Some(span.clone()),
                Some(current) if span.start == current.start && span.end > current.end => {
                    best = Some(span.clone());
                }
                Some(_) => {}
            }
        }

        best
    }

    /// Builds a scope set directly from spans, for testing the selection rules.
    ///
    /// The walk that normally fills this happens to yield spans outermost-first, so a rule stated
    /// to be independent of order would otherwise only ever be exercised in one order. The rules
    /// are what the suppression contract rests on, so they are tested as rules.
    #[cfg(test)]
    pub(super) fn from_spans(spans: Vec<Range<usize>>) -> Self {
        Self { spans, ..Self::default() }
    }

    /// Returns the span a directive trailing on a line of code governs.
    pub(super) fn enclosing_on_line(&self, line: usize) -> Option<Range<usize>> {
        self.lines.get(&line).cloned()
    }
}

/// Whether a span is one a directive could meaningfully govern.
///
/// An empty span governs no code, and one reaching past the end of the file came from a macro
/// expansion rather than from anything the author wrote. Attaching a directive to either would
/// suppress something the reader cannot see.
fn admissible(range: &Range<usize>, text_len: usize) -> bool {
    !range.is_empty() && range.end <= text_len
}

/// Walks a file gathering the spans a directive can attach to.
struct ScopeCollector {
    scopes: Scopes,
    text_len: usize,
}

impl ScopeCollector {
    /// Records a span if it lies inside the file text.
    fn record(&mut self, span: Span) -> Option<Range<usize>> {
        let range = span.byte_range();

        if !admissible(&range, self.text_len) {
            return None;
        }

        self.scopes.spans.push(range.clone());

        Some(range)
    }

    /// Records the attributes attached to a construct.
    fn record_attributes(&mut self, attributes: &[Attribute], span: Span) {
        let Some(range) = self.record(span) else {
            return;
        };

        for attribute in attributes {
            self.scopes.attributes.push((attribute.clone(), range.clone()));
        }
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for ScopeCollector {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.record_attributes(&node.attrs, node.span());
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        self.record_attributes(&node.attrs, node.span());
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        self.record_attributes(&node.attrs, node.span());
        visit::visit_trait_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        self.record_attributes(&node.attrs, node.span());
        visit::visit_item_impl(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        self.record_attributes(&node.attrs, node.span());
        visit::visit_item_mod(self, node);
    }

    fn visit_stmt(&mut self, node: &'ast Stmt) {
        let _ = self.record(node.span());
        visit::visit_stmt(self, node);
    }

    fn visit_expr(&mut self, node: &'ast Expr) {
        let _ = self.record(node.span());
        visit::visit_expr(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{directives, suppress};
    use super::*;
    use crate::model::Mutant;
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

    fn suppressed(source: &str, ops: &str) -> (usize, usize) {
        let (parsed, mut mutants) = mutants_of(source, ops);
        let found = directives(&parsed).unwrap();
        let count = suppress(&mut mutants, &found);

        (count, mutants.len())
    }

    #[test]
    fn a_comment_directive_suppresses_the_following_statement() {
        let source = "fn f(a: i32, b: i32) {\n    // #[gamma::skip(arith)]\n    let x = a + b;\n    let y = a - b;\n}";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(total, 4);
        assert_eq!(count, 2);
    }

    #[test]
    fn a_comment_directive_above_a_function_covers_the_whole_function() {
        let source = "// #[gamma::skip(arith)]\nfn f(a: i32, b: i32) -> i32 {\n    let x = a + b;\n    x - b\n}";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total > 0);
    }

    #[test]
    fn an_attribute_directive_covers_the_whole_function() {
        let source = "#[gamma::skip(arith)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total > 0);
    }

    #[test]
    fn a_trailing_directive_governs_its_own_line() {
        let source = "fn f(a: i32, b: i32) {\n    let x = a + b; // #[gamma::skip(arith)]\n    let y = a - b;\n}";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(total, 4);
        assert_eq!(count, 2);
    }

    #[test]
    fn one_line_function_trailing_directive_prefers_the_widest_scope() {
        let source = "fn f(a: i32, b: i32) -> i32 { a + b } // #[gamma::skip(arith)]";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total > 0);
    }

    #[test]
    fn following_prefers_the_widest_scope_at_the_earliest_start() {
        let parsed = file("// directive\nfn f(a: i32, b: i32) -> i32 { a + b }\n");
        let scopes = Scopes::of(&parsed);
        let scope = scopes.following(0).unwrap();

        assert_eq!(parsed.slice(&scope), "fn f(a: i32, b: i32) -> i32 { a + b }");
    }

    #[test]
    fn directives_attach_to_impl_trait_and_module_scopes() {
        let source = "trait T {
            // #[gamma::skip(arith)]
            fn f(&self) -> i32 { 1 + 1 }
        }
        struct S;
        impl S {
            // #[gamma::skip(arith)]
            fn g(&self) -> i32 { 2 + 2 }
        }
        // #[gamma::skip(arith)]
        impl T for S { fn f(&self) -> i32 { 3 + 3 } }
        // #[gamma::skip(arith)]
        mod m { pub fn h() -> i32 { 4 + 4 } }";
        let (count, total) = suppressed(source, "arith");

        assert_eq!(count, total);
        assert!(total >= 8, "{total}");
    }

    /// A line holding two constructs is governed by the wider of them, not the first one seen.
    #[test]
    fn the_widest_span_starting_on_a_line_wins() {
        let source = "fn a() { let _ = 1 + 1; } fn bbbbb() { let _ = 2 + 2; let _ = 3 + 3; }\n";
        let parsed = file(source);
        let scopes = Scopes::of(&parsed);
        let governing = scopes.enclosing_on_line(1).expect("a span on the only line");

        // The second function ends later than the first, so it is what a trailing directive on
        // this line has to govern.
        assert_eq!(governing.end, source.trim_end().len());
    }

    /// The widest span at a shared start wins whichever order the spans arrive in.
    #[test]
    fn the_outermost_span_at_a_shared_start_wins_in_either_order() {
        let inner_first = Scopes::from_spans(vec![0..10, 0..40, 0..25]);
        let outer_first = Scopes::from_spans(vec![0..40, 0..25, 0..10]);

        // A directive above a nested construct means the outer one; the traversal that normally
        // fills this yields spans outermost-first, so the rule must not depend on that.
        assert_eq!(inner_first.following(0), Some(0..40));
        assert_eq!(outer_first.following(0), Some(0..40));
    }

    /// The earliest span at or after the offset wins, whatever order they arrive in.
    #[test]
    fn the_nearest_span_after_the_offset_wins() {
        let scopes = Scopes::from_spans(vec![50..60, 20..30, 5..8]);

        // A directive governs what follows it, which is the construct that starts soonest after
        // it rather than the widest one anywhere below.
        assert_eq!(scopes.following(10), Some(20..30));
        assert_eq!(scopes.following(61), None);
    }

    /// A span governing nothing visible is never recorded.
    #[test]
    fn an_empty_or_overrunning_span_is_not_a_scope() {
        // An empty span governs no code, and one past the end of the file came from a macro
        // expansion; a directive attached to either would suppress something invisible.
        assert!(!admissible(&(5..5), 100));
        assert!(!admissible(&(90..120), 100));
        assert!(admissible(&(90..100), 100));
    }

    /// A directive above two constructs that begin together governs the outer one.
    #[test]
    fn the_outermost_span_from_a_real_parse_is_the_governing_one() {
        let source = "fn f() {\n    let _ = 1;\n}\n";
        let parsed = file(source);
        let scopes = Scopes::of(&parsed);
        let governing = scopes.following(0).expect("a span after the start of the file");

        assert_eq!(governing, 0..source.trim_end().len());
    }
}
