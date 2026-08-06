use core::fmt::Display;

use proc_macro2::Span;
use syn::spanned::Spanned as _;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::visit::{self, Visit};
use syn::{
    Attribute, BinOp, Block, Expr, ExprBinary, ExprBreak, ExprCall, ExprContinue, ExprIf, ExprIndex, ExprLit,
    ExprMatch, ExprMethodCall, ExprRange, ExprReference, ExprRepeat, ExprReturn, ExprStruct, ExprUnary, ExprWhile,
    FnArg, GenericArgument, GenericParam, Generics, ImplItemConst, ImplItemFn, ItemConst, ItemFn, ItemImpl, ItemMod, ItemStatic, Lit,
    Local, Macro, Pat, PathArguments, RangeLimits, ReturnType, Signature, Stmt, TraitItemConst, TraitItemFn, Type,
    TypeParamBound, UnOp, Variant,
};

use crate::HashMap;
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

use super::{Candidate, Shape};

/// The marker string every string-valued mutant is replaced by.
///
/// It is deliberately implausible: a test that happens to accept it was almost certainly not
/// asserting on the string at all.
const XYZZY: &str = "xyzzy";

/// The traversal state.
pub(super) struct Collector<'a> {
    file: &'a SourceFile,
    selection: &'a Selection,

    /// The enclosing item path at each nesting level, each entry already fully joined.
    ///
    /// Storing the joined form rather than the segments means emitting a candidate copies one
    /// string instead of rebuilding the path from its parts, and a large tree emits far more
    /// candidates than it opens scopes.
    scope: Vec<String>,

    candidates: Vec<Candidate>,

    /// Depth of nesting inside a context where mutation is not possible or not useful.
    ///
    /// Const and static initializers are the important case: the encoding wraps the original
    /// expression in an `if` over a function call, which is not permitted in a const context. A
    /// mutant that cannot compile is not a weak test, it is noise, so these are never generated
    /// rather than generated and then rolled back.
    inert_depth: usize,

    /// Caller-supplied `Err(...)` payloads, from `--error`.
    errors: &'a [String],

    /// Whether the function being traversed returns a number.
    ///
    /// Perturbing a returned value only makes sense when the value is one that can be off by one.
    /// Without this the family offered `(Vec::new()) + 1` for every function returning a
    /// collection, and each of those costs a rollback round to discover it never compiled.
    numeric_return: bool,

    /// Whether each named binding in the enclosing function holds a number.
    ///
    /// The perturbation family adds one to an expression, and the commonest thing it is offered is
    /// a bare identifier — for which nothing in the syntax says whether it is an integer or a
    /// `String`. Function parameters and annotated `let`s are the two places a local type is
    /// written down in the source, and recording them converts most of those guesses into an
    /// answer. A binding that is not here was never written down, and is left alone rather than
    /// assumed either way.
    bindings: HashMap<String, bool>,

    /// Type parameters in scope that are not known to implement `Default`.
    ///
    /// The `fn_value` family reaches for `Default::default()` whenever it cannot name a value of a
    /// type, and for an abstract type that is a guess rather than a fact: nothing says a caller's
    /// `E` or a trait's `Self::Value` has a `Default`, and on a serde-shaped API almost none of
    /// them do. Holding the names lets the guess be withheld exactly where it is unfounded.
    generics: Vec<String>,

    /// The configuration predicates that hold for the build this file will be part of.
    ///
    /// Code behind a predicate that does not hold is stripped by the compiler, so a guard there is
    /// never compiled, no test can activate it, and the mutant would be reported as a survivor no
    /// test could ever have caught.
    cfg: &'a CfgSet,
}

impl<'a> Collector<'a> {
    /// Creates a collector rooted at the file's top-level scope.
    pub(super) fn new(
        file: &'a SourceFile,
        selection: &'a Selection,
        errors: &'a [String],
        cfg: &'a CfgSet,
    ) -> Self {
        Self {
            file,
            selection,
            scope: Vec::new(),
            candidates: Vec::new(),
            inert_depth: 0,
            numeric_return: false,
            bindings: HashMap::default(),
            generics: Vec::new(),
            errors,
            cfg,
        }
    }

    /// Consumes the collector, returning what it found.
    pub(super) fn finish(self) -> Vec<Candidate> {
        self.candidates
    }

    /// Records an expression-shaped candidate if its mutator is selected.
    fn emit(&mut self, mutator: &'static str, span: Span, replacement: impl Into<String>, replacement_index: u32) {
        self.emit_shaped(mutator, span, replacement, replacement_index, Shape::Expr);
    }

    /// Records a candidate of a given shape if its mutator is selected.
    fn emit_shaped(
        &mut self,
        mutator: &'static str,
        span: Span,
        replacement: impl Into<String>,
        replacement_index: u32,
        shape: Shape,
    ) {
        if !self.wants(mutator) {
            return;
        }

        self.emit_at(mutator, span.byte_range(), replacement, replacement_index, shape);
    }

    /// Records a candidate over an explicit byte range.
    ///
    /// Most sites come from a single syntax node and can use its span, but a site can also span
    /// several nodes — the elements of a `vec!` from the first to the last — with no one node
    /// covering exactly the text being replaced.
    fn emit_at(
        &mut self,
        mutator: &'static str,
        range: core::ops::Range<usize>,
        replacement: impl Into<String>,
        replacement_index: u32,
        shape: Shape,
    ) {
        if !self.wants(mutator) {
            return;
        }

        // A range outside the file text means the node came from a macro expansion, where byte
        // offsets do not correspond to anything we can splice.
        if range.start >= range.end || range.end > self.file.text.len() {
            return;
        }

        self.candidates.push(Candidate {
            mutator,
            span: range,
            replacement: replacement.into(),
            replacement_index,
            item_path: self.scope.last().cloned().unwrap_or_default(),
            shape,
        });
    }

    /// Returns whether a mutator would produce anything here.
    ///
    /// Checked before building any replacement text, so a tree scanned with a mutator switched off
    /// pays nothing for the text it would have spliced.
    fn wants(&self, mutator: &str) -> bool {
        self.inert_depth == 0 && self.selection.contains(mutator)
    }

    /// Returns the source text a span covers, or an empty string if the span is not in the file.
    ///
    /// Borrowed from the file rather than the collector, so a caller can hold the result across a
    /// mutating call and no copy is made for a span that turns out not to be wanted.
    fn text_of(&self, span: Span) -> &'a str {
        self.file.text.get(span.byte_range()).unwrap_or("")
    }

    /// Returns the negation of the expression a span covers.
    ///
    /// The parentheses are not optional. `!` binds tighter than every binary operator, so negating
    /// `a == b` without them yields `!a == b`, which is a different expression and usually does
    /// not even type-check.
    fn negation_of(&self, span: Span) -> String {
        format!("!({})", self.text_of(span))
    }

    /// Runs a closure with a name pushed onto the scope stack.
    fn scoped<T>(&mut self, name: impl Display, body: impl FnOnce(&mut Self) -> T) -> T {
        let path = self
            .scope
            .last()
            .map_or_else(|| name.to_string(), |parent| format!("{parent}::{name}"));

        self.scope.push(path);

        let result = body(self);
        let _ = self.scope.pop();

        result
    }

    /// Emits the function-value mutants for a function with the given signature and body.
    ///
    /// This is the family `cargo-mutants` is built around, and it earns its place: replacing a
    /// whole function body with a plausible constant asks the bluntest possible question about a
    /// test suite, which is whether it looks at the answer at all.
    fn function(&mut self, sig: &Signature, body: &Block) {
        // The whole body of a `const fn` is a const context, so nothing in it can call the guard
        // predicate. `visit_const_fn` keeps the subtree inert; this only guards the body value.
        if sig.constness.is_some() {
            return;
        }

        // An empty body already produces the unit value, so replacing it with one changes nothing.
        // A mutant that cannot alter behavior can never be caught, and reporting it as a survivor
        // would be an accusation against the test suite for something no test could detect.
        if body.stmts.is_empty() {
            return;
        }

        let span = body.span();

        // A method's own type parameters join the ones its `impl` block declares; both are in
        // scope in the signature being read here.
        let mut abstracts = self.generics.clone();

        abstracts.extend(undefaulted_parameters(&sig.generics));

        let values = return_values(&sig.output, &abstracts);

        // The value a function ends on is the one its caller reasons about, so it is one of the
        // positions where being wrong by one is a real fault rather than a compile error — but
        // only when the value is a number, which the signature already says.
        if is_numeric_return(&sig.output)
            && let Some(Stmt::Expr(trailing, None)) = body.stmts.last()
        {
            self.perturb(trailing);
        }

        for (index, (mutator, value)) in values.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit_shaped(mutator, span, value.as_str(), index, Shape::Block);
        }

        // Caller-supplied error values, which reach the error types `Err(Default::default())`
        // cannot. Their indices continue the static list's so that adding one does not renumber
        // the mutants already generated at this site.
        if returns_result(&sig.output) && self.wants("fn_value.err_with") {
            for (offset, error) in self.errors.iter().enumerate() {
                let index = u32::try_from(values.len().saturating_add(offset)).unwrap_or(u32::MAX);
                let replacement = format!("Err({error})");

                self.emit_shaped("fn_value.err_with", span, &replacement, index, Shape::Block);
            }
        }
    }

    /// Emits the statement-deletion mutants for one statement.
    ///
    /// Only statements whose value is discarded are eligible. Deleting a `let` would leave every
    /// later use of the binding unresolved, which is a compile error rather than a mutant, and
    /// deleting a block's trailing expression would change the block's type.
    fn statement(&mut self, statement: &Stmt) {
        let Stmt::Expr(expression, Some(_)) = statement else {
            return;
        };

        let mutator = match expression {
            // A call whose result is thrown away is being run for its effect, which is exactly the
            // thing a test that only checks return values will not notice going missing.
            Expr::Call(_) | Expr::MethodCall(_) => in_place_reorder(expression).unwrap_or("stmt.delete_call"),
            Expr::Assign(_) => "stmt.delete_assign",
            Expr::Binary(binary) if is_assign_op(&binary.op) => "stmt.delete_assign",

            // A `break` carrying a value decides the type of the loop it leaves, so deleting it
            // can change that type rather than the program's behaviour.
            Expr::Break(brk) if brk.expr.is_none() => "loop.delete_break",
            Expr::Continue(_) => "loop.delete_continue",

            _ => return,
        };

        self.emit_shaped(mutator, statement.span(), "", 0, Shape::Stmt);
    }

    /// Emits the guard mutants for one boolean condition.
    ///
    /// Shared by `if`, `while` and match arms, which ask the same question in three syntaxes and
    /// should not answer it three different ways.
    fn condition(&mut self, negate: &'static str, always_true: &'static str, always_false: &'static str, cond: &Expr) {
        // A condition that binds a pattern cannot be negated or replaced by a boolean. In a let
        // chain the binding may sit anywhere in the `&&` spine, not just at the top.
        if binds_a_pattern(cond) {
            return;
        }

        let span = cond.span();

        if self.wants(negate) {
            let negated = self.negation_of(span);

            self.emit(negate, span, negated, 0);
        }

        // A condition that is already the literal it would be replaced by yields a mutant that
        // compiles to the original program, so it can never be caught and would be scored as a
        // survivor forever.
        let literal = boolean_literal(cond);

        if literal != Some(true) {
            self.emit(always_true, span, "true", 1);
        }

        if literal != Some(false) {
            self.emit(always_false, span, "false", 2);
        }
    }

    /// Emits the mutants for the arms of one `match`.
    ///
    /// Two unrelated families meet here. An arm with a guard is a condition like any other and is
    /// mutated as one. An arm without a guard can instead be made to stop matching, but only when
    /// a later wildcard is there to receive what falls through — the compiler does not count a
    /// guarded arm towards exhaustiveness, so adding a guard to the last arm that can match a
    /// value turns the mutant into a compile error rather than a question about the tests.
    fn match_arms(&mut self, node: &ExprMatch) {
        // The first wildcard, and only an unguarded one: a guarded `_` catches nothing in
        // particular and leaves the match relying on the arms above it.
        let wildcard = node
            .arms
            .iter()
            .position(|arm| matches!(arm.pat, Pat::Wild(_)) && arm.guard.is_none());

        for (index, arm) in node.arms.iter().enumerate() {
            if self.skipped(&arm.attrs) {
                continue;
            }

            if let Some((_if, guard)) = arm.guard.as_ref() {
                self.condition(
                    "match_guard.negate",
                    "match_guard.always_true",
                    "match_guard.always_false",
                    guard,
                );

                // An arm that already has a guard is disabled by forcing that guard false, which
                // `match_guard.always_false` above already offers. Emitting a second mutant that
                // does the same thing would pay twice for one question.
                continue;
            }

            if wildcard.is_some_and(|at| index < at) {
                self.emit_shaped("match_arm.never_matches", arm.pat.span(), "", 0, Shape::Arm);
            }
        }
    }

    /// Emits the field-omission mutants for one struct literal.
    ///
    /// Only a literal with a base expression is eligible, because the base is what keeps the
    /// result well formed once a field is taken out. Each mutant asks whether any test can tell
    /// the written value from the one the base would have supplied — which, for a field that is
    /// being set to its default anyway, nothing can.
    fn struct_fields(&mut self, node: &ExprStruct) {
        if !self.wants("struct_field.omit") {
            return;
        }

        // The `..` token rather than the expression after it, since the field being removed runs
        // up to the token and the whitespace between them belongs to neither.
        let Some(rest) = node.dot2_token.as_ref().map(syn::spanned::Spanned::span) else {
            return;
        };

        let whole = node.span().byte_range();

        if whole.start >= whole.end || whole.end > self.file.text.len() {
            return;
        }

        for (index, field) in node.fields.iter().enumerate() {
            // A field behind a predicate that does not hold is not in the compiled literal, so
            // omitting it changes nothing that could be observed.
            if self.skipped(&field.attrs) {
                continue;
            }

            let from = field.span().byte_range().start;
            let to = node
                .fields
                .iter()
                .nth(index.saturating_add(1))
                .map_or_else(|| rest.byte_range().start, |next| next.span().byte_range().start);

            // Everything between the two is the field, its comma and the space after it.
            if from < whole.start || to > whole.end || from >= to {
                continue;
            }

            let (Some(head), Some(tail)) = (self.file.text.get(whole.start..from), self.file.text.get(to..whole.end))
            else {
                continue;
            };

            let replacement = format!("{head}{tail}");
            let ordinal = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit_shaped("struct_field.omit", node.span(), replacement, ordinal, Shape::Expr);
        }
    }

    /// Offers `+ 1` and `- 1` for one expression, in a position where a boundary is being decided.
    ///
    /// Deliberately not applied to every expression. Doing so would double the population of a
    /// large project, duplicate the literal and arithmetic families wherever they already apply,
    /// and produce type errors anywhere the expression is generic or not numeric at all. The
    /// positions it is applied to are the ones that carry a postcondition somebody could get
    /// wrong by one: what a function is handed, what it gives back, what is indexed, and where a
    /// range stops.
    /// Offers a mutant for each element of a `vec!` literal, with that element removed.
    ///
    /// The removal sweeps up the separating comma along with the element, so the list that is left
    /// is still well formed wherever the element sat in it.
    fn omit_elements(&mut self, node: &Macro, elements: &Punctuated<Expr, Comma>) {
        let spans: Vec<_> = elements.iter().map(|element| element.span().byte_range()).collect();

        let (Some(first), Some(last)) = (spans.first(), spans.last()) else {
            return;
        };

        // The site has to be the whole `vec![..]`, not just the elements inside it. A guarded
        // mutant is one arm of an `if`, and `1, 2` is a list rather than an expression, so
        // narrowing the site to the element range would emit code that does not parse at all.
        let whole = node.span().byte_range();
        let items = first.start..last.end;

        let (Some(text), Some(head), Some(tail)) = (
            self.file.text.get(items.clone()),
            self.file.text.get(whole.start..items.start),
            self.file.text.get(items.end..whole.end),
        ) else {
            return;
        };

        let (text, head, tail) = (text.to_owned(), head.to_owned(), tail.to_owned());

        for (index, span) in spans.iter().enumerate() {
            // Everything up to this element, and everything from the next one on. For the final
            // element there is no next, so the cut runs to the end and takes the preceding comma
            // with it.
            let (from, to) = spans.get(index.saturating_add(1)).map_or_else(
                || (spans.get(index.wrapping_sub(1)).map_or(span.start, |previous| previous.end), items.end),
                |next| (span.start, next.start),
            );

            let (Some(before), Some(after)) =
                (text.get(..from.saturating_sub(items.start)), text.get(to.saturating_sub(items.start)..))
            else {
                continue;
            };

            let replacement = format!("{head}{before}{after}{tail}");
            let ordinal = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit_at(
                "collection.omit_element",
                whole.clone(),
                replacement,
                ordinal,
                Shape::Expr,
            );
        }
    }

    /// Offers the curated same-shape renames of a standard-library method.
    ///
    /// The whole call expression is the site, not the method name, because a mutant is spliced in
    /// as `if guard { .. } else { .. }` and an `if` is not a legal method name. Rewriting the name
    /// inside a copy of the call's own text keeps any turbofish and every argument exactly as
    /// written, which reconstructing the call from its parts would not.
    fn rename_method(&mut self, node: &ExprMethodCall, method: &str) {
        let Some(swaps) = method_renames(method, node.args.len()) else {
            return;
        };

        let whole = node.span().byte_range();
        let name = node.method.span().byte_range();

        // A method call whose receiver spans a macro expansion can put the name outside the call,
        // in which case there is nothing meaningful to splice.
        if name.start < whole.start || name.end > whole.end {
            return;
        }

        let text = &self.file.text;
        let (Some(before), Some(after)) = (text.get(whole.start..name.start), text.get(name.end..whole.end)) else {
            return;
        };

        let (before, after) = (before.to_owned(), after.to_owned());

        for (index, (mutator, replacement)) in swaps.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit(mutator, node.span(), format!("{before}{replacement}{after}"), index);
        }
    }

    fn perturb(&mut self, expression: &Expr) {
        if !self.wants("expr.increment") && !self.wants("expr.decrement") {
            return;
        }

        if !is_perturbable(expression) || is_capacity_result(expression) {
            return;
        }

        if self.is_known_non_numeric(expression) {
            return;
        }

        let span = expression.span();
        let text = self.text_of(span);

        if text.is_empty() {
            return;
        }

        // Parenthesised because the expression may bind more loosely than the addition, and
        // because the result is spliced into whatever position the original held.
        let incremented = format!("({text}) + 1");
        let decremented = format!("({text}) - 1");

        self.emit("expr.increment", span, incremented, 0);
        self.emit("expr.decrement", span, decremented, 1);
    }

    /// Returns whether a bare identifier names a local the source says is not a number.
    ///
    /// Only a written-down type counts. An identifier with no binding on record is left alone,
    /// because the cost of the two answers is not the same: a mutant that cannot compile costs a
    /// share of one rebuild, while a viable mutant skipped on a guess is a hole in the report that
    /// nothing else would ever reveal.
    fn is_known_non_numeric(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Path(path) if path.qself.is_none() => path
                .path
                .get_ident()
                .and_then(|ident| self.bindings.get(&ident.to_string()))
                .is_some_and(|numeric| !numeric),

            Expr::Paren(paren) => self.is_known_non_numeric(&paren.expr),
            Expr::Group(group) => self.is_known_non_numeric(&group.expr),

            _ => false,
        }
    }

    /// Offers the perturbations for every argument of a call, unless the callee is one whose
    /// arguments are a performance decision rather than a behavioural one.
    fn perturb_arguments(&mut self, callee: Option<&str>, args: &Punctuated<Expr, Comma>) {
        if callee.is_some_and(is_capacity_call) {
            return;
        }

        for argument in args {
            self.perturb(argument);
        }
    }

    /// Returns whether an item's attributes take it out of the population entirely.
    ///
    /// Two unrelated reasons land here: the item is test code, or it is behind a configuration
    /// predicate that does not hold for this build. Both mean the same thing to the collector —
    /// do not descend — so they are asked together at every place that can be entered.
    fn skipped(&self, attrs: &[Attribute]) -> bool {
        is_excluded(attrs) || !self.cfg.holds_for(attrs)
    }

    /// Runs `body`, treating everything it visits as inert when `constant` holds.
    ///
    /// A guard is a function call, which const contexts disallow; mutants generated there would
    /// never compile and end up withdrawn as noise instead of measuring anything.
    fn in_const<T>(&mut self, constant: bool, body: impl FnOnce(&mut Self) -> T) -> T {
        if constant {
            self.inert(body)
        } else {
            body(self)
        }
    }

    /// Runs `body` with the enclosing function's return type recorded.
    ///
    /// Restored rather than cleared afterwards, because a nested function inside a body must not
    /// leave the outer one's `return` expressions looking like its own.
    fn in_function<T>(&mut self, sig: &Signature, body: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.numeric_return;

        self.numeric_return = is_numeric_return(&sig.output);

        // Saved and restored rather than cleared, because a function defined inside another one
        // cannot see the outer function's locals and must not be allowed to reason from them.
        let outer_bindings = core::mem::take(&mut self.bindings);

        for input in &sig.inputs {
            let FnArg::Typed(typed) = input else {
                continue;
            };

            if let Pat::Ident(ident) = &*typed.pat {
                let _replaced = self.bindings.insert(ident.ident.to_string(), is_numeric_binding(&typed.ty));
            }
        }

        let result = body(self);

        self.bindings = outer_bindings;
        self.numeric_return = outer;
        result
    }

    fn inert<T>(&mut self, body: impl FnOnce(&mut Self) -> T) -> T {
        self.inert_depth += 1;

        let result = body(self);

        self.inert_depth -= 1;
        result
    }
}

/// Returns whether any attribute suppresses mutation of the whole item.
fn is_excluded(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();

        // `#[cfg(test)]` marks code that exists only to test other code. Mutating it measures the
        // tests' tests, which nobody has.
        if path.is_ident("cfg") {
            let mut is_test = false;

            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("test") {
                    is_test = true;
                }

                Ok(())
            });

            if is_test {
                return true;
            }
        }

        // `#[test]`, `#[tokio::test]`, and anything else whose last segment is `test`.
        path.segments.last().is_some_and(|segment| segment.ident == "test")
    })
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for Collector<'_> {
    /// Records the type of an annotated `let`, so that later uses of the name can be judged.
    ///
    /// Statements are visited in source order, so a name resolves to the most recent binding that
    /// precedes the use, which is what shadowing means. A `let` with no annotation is left off the
    /// record rather than guessed at.
    fn visit_local(&mut self, node: &'ast Local) {
        if let Pat::Type(typed) = &node.pat
            && let Pat::Ident(ident) = &*typed.pat
        {
            let _replaced = self.bindings.insert(ident.ident.to_string(), is_numeric_binding(&typed.ty));
        }

        visit::visit_local(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.sig.ident, |collector| {
            collector.function(&node.sig, &node.block);
            collector.in_function(&node.sig, |collector| {
                collector.in_const(node.sig.constness.is_some(), |collector| {
                    visit::visit_item_fn(collector, node);
                });
            });
        });
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.sig.ident, |collector| {
            collector.function(&node.sig, &node.block);
            collector.in_function(&node.sig, |collector| {
                collector.in_const(node.sig.constness.is_some(), |collector| {
                    visit::visit_impl_item_fn(collector, node);
                });
            });
        });
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.ident, |collector| visit::visit_item_mod(collector, node));
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if self.skipped(&node.attrs) {
            return;
        }

        let depth = self.generics.len();

        self.generics.extend(undefaulted_parameters(&node.generics));
        self.scoped(type_name(&node.self_ty), |collector| visit::visit_item_impl(collector, node));
        self.generics.truncate(depth);
    }

    fn visit_item_const(&mut self, node: &'ast ItemConst) {
        self.inert(|collector| visit::visit_item_const(collector, node));
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        self.inert(|collector| visit::visit_item_static(collector, node));
    }

    fn visit_impl_item_const(&mut self, node: &'ast ImplItemConst) {
        self.inert(|collector| visit::visit_impl_item_const(collector, node));
    }

    fn visit_trait_item_const(&mut self, node: &'ast TraitItemConst) {
        self.inert(|collector| visit::visit_trait_item_const(collector, node));
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        // The expansion is not visible here and its spans do not map back onto the source, so
        // nothing inside a macro is traversed. `vec![a, b, c]` is the exception worth making: the
        // elements are written literally at the call site, so their spans are ordinary source
        // spans and removing one is an ordinary splice.
        //
        // `vec![value; count]` is excluded for free, because it does not parse as a comma-
        // separated list. Arrays are excluded deliberately: an array's length is part of its type,
        // so dropping an element changes the type rather than the behavior.
        if !node.path.is_ident("vec") {
            return;
        }

        let Ok(elements) = node.parse_body_with(Punctuated::<Expr, Comma>::parse_terminated) else {
            return;
        };

        // One element and the list would become empty, which is a different question — whether the
        // collection is needed at all — and one that `Vec::new()` already asks of the function.
        if elements.len() < 2 {
            return;
        }

        self.omit_elements(node, &elements);
    }

    fn visit_attribute(&mut self, _node: &'ast Attribute) {
        // Attributes are metadata, not behavior. This matters more than it sounds: a doc comment
        // is desugared into `#[doc = "..."]`, so without this every line of documentation in the
        // tree would present itself as a mutable string literal.
    }

    fn visit_pat(&mut self, _node: &'ast Pat) {
        // A pattern is matched against, not evaluated, so nothing in one can be guarded: a guard
        // is an `if` expression and no expression is legal in pattern position. This matters
        // because `syn` models a literal pattern as an `ExprLit`, so without this every `"skip"
        // =>` match arm would offer itself as a mutable literal and produce a mutant that cannot
        // compile.
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        let span = node.span();
        let assigns = is_assign_op(&node.op);
        let mut operands = None;

        // A `&&` that is part of a let-chain is not an ordinary boolean operator: turning it into
        // `||` is rejected by the parser, because a binding cannot escape one arm of an `or`. The
        // binding may be on either side, as the chain associates to the left and a `let` commonly
        // comes last.
        let chains_a_binding =
            matches!(node.op, BinOp::And(_)) && (binds_a_pattern(&node.left) || binds_a_pattern(&node.right));

        for (index, (mutator, operator)) in binary_replacements(&node.op).iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);

            // Most binary expressions in a tree have no selected mutator, and the operand text is
            // only needed to build a replacement, so it is read once and only on demand.
            if chains_a_binding || !self.wants(mutator) {
                continue;
            }

            let (left, right) =
                *operands.get_or_insert_with(|| (self.text_of(node.left.span()), self.text_of(node.right.span())));

            // The operands are parenthesized because the replacement is spliced in as a unit and
            // must not renegotiate precedence with whatever encloses it: rewriting the `*` in
            // `a + b * c` to `+` has to keep `b + c` grouped. The left side of a compound
            // assignment is left alone, since it is a place expression and reads better bare.
            let replacement = if assigns {
                format!("{left} {operator} ({right})")
            } else {
                format!("({left}) {operator} ({right})")
            };

            self.emit(mutator, span, replacement, index);
        }

        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_repeat(&mut self, node: &'ast ExprRepeat) {
        // The length of `[0u8; 32]` is a const expression, so it cannot hold a guard, but the
        // element expression can.
        self.visit_expr(&node.expr);
        self.inert(|collector| collector.visit_expr(&node.len));
    }

    fn visit_expr_reference(&mut self, node: &'ast ExprReference) {
        // `fn f() -> &'static [&'static str] { &["a", "b"] }` compiles only because the borrowed
        // array is a constant, which lets it be promoted to static storage. A guard is a function
        // call, so instrumenting anything inside one stops it being constant, the array becomes an
        // ordinary temporary, and the borrow no longer outlives the function. The result is a
        // borrow-check error over the whole enclosing expression rather than at the mutated site.
        self.in_const(is_promotable(&node.expr), |collector| {
            visit::visit_expr_reference(collector, node);
        });
    }

    fn visit_variant(&mut self, node: &'ast Variant) {
        // An enum discriminant is a const expression.
        self.inert(|collector| visit::visit_variant(collector, node));
    }

    fn visit_type(&mut self, node: &'ast Type) {
        // Every expression reachable from inside a type is a const expression: the length in
        // `[u8; 200]`, the argument in `Matrix<3, 3>`, and the same two nested arbitrarily deep in
        // a field, a return type or a `where` clause. `visit_expr_repeat` covers `[0u8; 32]`, the
        // *value*, and it is easy to assume that is the same thing — it is not, and the difference
        // is a mutant that cannot compile in a position the rollback rounds then have to discover
        // by building the whole tree.
        self.inert(|collector| visit::visit_type(collector, node));
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.sig.ident, |collector| {
            if let Some(body) = node.default.as_ref() {
                collector.function(&node.sig, body);
            }

            collector.in_function(&node.sig, |collector| {
                collector.in_const(node.sig.constness.is_some(), |collector| {
                    visit::visit_trait_item_fn(collector, node);
                });
            });
        });
    }

    fn visit_block(&mut self, node: &'ast Block) {
        for statement in &node.stmts {
            self.statement(statement);
        }

        visit::visit_block(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast ExprUnary) {
        // `*` is by far the most common unary operator and has no mutant, so the operand text is
        // never read for one.
        match node.op {
            // Negating zero yields zero, so removing the negation changes nothing.
            UnOp::Neg(_) if is_zero_literal(&node.expr) => {}
            UnOp::Neg(_) => self.emit("unary.remove_neg", node.span(), self.text_of(node.expr.span()), 0),
            UnOp::Not(_) => self.emit("unary.remove_not", node.span(), self.text_of(node.expr.span()), 0),
            _ => {}
        }

        visit::visit_expr_unary(self, node);
    }

    fn visit_expr_if(&mut self, node: &'ast ExprIf) {
        self.condition("cond.negate", "cond.always_true", "cond.always_false", &node.cond);

        visit::visit_expr_if(self, node);
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        // Only the negation, because a `while` forced to `true` never terminates and one forced to
        // `false` is the loop deleted — the first costs a full timeout to reach a verdict already
        // available more cheaply, and the second is what statement deletion asks.
        if !binds_a_pattern(&node.cond) && self.wants("cond.negate") {
            let span = node.cond.span();
            let negated = self.negation_of(span);

            self.emit("cond.negate", span, negated, 0);
        }

        visit::visit_expr_while(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        self.match_arms(node);

        visit::visit_expr_match(self, node);
    }

    fn visit_expr_struct(&mut self, node: &'ast ExprStruct) {
        self.struct_fields(node);

        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_range(&mut self, node: &'ast ExprRange) {
        // A range with no end has no boundary to move, and one with no start is still a boundary
        // worth moving, so only the end is required.
        if let Some(end) = node.end.as_ref() {
            let start = node.start.as_ref().map_or_else(String::new, |start| {
                let text = self.text_of(start.span());

                format!("({text})")
            });

            let end_text = self.text_of(end.span());

            // The change is expressed by moving the endpoint rather than by swapping `..` for
            // `..=`, even though swapping is what the mutation means. Every mutant here is run by
            // wrapping the site as `if guard { mutant } else { original }`, and the two arms of an
            // `if` must have the same type. `Range` and `RangeInclusive` are different types, so a
            // literal swap cannot compile — not occasionally, but every single time, which would
            // make the whole family a guaranteed build round spent to withdraw itself.
            //
            // `a..b + 1` covers exactly what `a..=b` covers and `a..=b - 1` covers exactly what
            // `a..b` covers, so the question put to the suite is unchanged. On an unsigned
            // endpoint that is already zero the subtraction overflows and the mutant is caught by
            // the panic, which is the right answer for the wrong reason but still the right answer.
            //
            // Parenthesised for the same reason every other replacement here is: the operands are
            // spliced back into an expression whose precedence we do not control.
            match node.limits {
                RangeLimits::HalfOpen(_) => {
                    self.emit("range.exclusive_to_inclusive", node.span(), format!("{start}..(({end_text}) + 1)"), 0);
                }
                RangeLimits::Closed(_) => {
                    self.emit("range.inclusive_to_exclusive", node.span(), format!("{start}..=(({end_text}) - 1)"), 0);
                }
            }

            self.perturb(end);
        }

        if let Some(start) = node.start.as_ref() {
            self.perturb(start);
        }

        visit::visit_expr_range(self, node);
    }

    fn visit_expr_break(&mut self, node: &'ast ExprBreak) {
        // A labelled `break` may be leaving a labelled block rather than a loop, and `continue`
        // cannot leave a block. A `break` carrying a value decides the type of its loop, which
        // `continue` cannot supply.
        if node.expr.is_none() && node.label.is_none() {
            self.emit("loop.break_to_continue", node.span(), "continue", 0);
        }

        visit::visit_expr_break(self, node);
    }

    fn visit_expr_continue(&mut self, node: &'ast ExprContinue) {
        // A label on a `continue` can only name a loop, so the same label is always valid on a
        // `break`.
        let replacement = node
            .label
            .as_ref()
            .map_or_else(|| "break".to_owned(), |label| format!("break {label}"));

        self.emit("loop.continue_to_break", node.span(), replacement, 0);

        visit::visit_expr_continue(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        self.perturb_arguments(callee_name(&node.func).as_deref(), &node.args);

        // `Some(v)`, `Ok(v)` and `Err(v)` decide, at the point it is decided, whether a value is
        // present and whether an operation succeeded. Replacing a whole function can only ask that
        // question a function at a time; this asks it at the site.
        if node.args.len() == 1 {
            match callee_name(&node.func).as_deref() {
                Some("Some") => self.emit("option.some_to_none", node.span(), "None", 0),
                Some("Ok") => self.emit("result.ok_to_err", node.span(), "Err(Default::default())", 0),
                Some("Err") => self.emit("result.err_to_ok", node.span(), "Ok(Default::default())", 0),
                _ => {}
            }
        }

        visit::visit_expr_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        // A bare `None` in expression position. Patterns never reach here, because `visit_pat`
        // stops the traversal before a pattern's interior is examined at all.
        if node.path.is_ident("None") {
            self.emit("option.none_to_some", node.span(), "Some(Default::default())", 0);
        }

        visit::visit_expr_path(self, node);
    }

    fn visit_expr_assign(&mut self, node: &'ast syn::ExprAssign) {
        // What is assigned is what the rest of the function reads, so replacing it with the type's
        // default asks whether anything downstream depends on the value rather than the write.
        if !is_default_call(&node.right) {
            self.emit("assign_value.default", node.right.span(), "Default::default()", 0);
        }

        visit::visit_expr_assign(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method = node.method.to_string();

        self.perturb_arguments(Some(method.as_str()), &node.args);
        self.rename_method(node, &method);

        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast ExprIndex) {
        self.perturb(&node.index);

        visit::visit_expr_index(self, node);
    }

    fn visit_expr_return(&mut self, node: &'ast ExprReturn) {
        if self.numeric_return
            && let Some(value) = node.expr.as_ref()
        {
            self.perturb(value);
        }

        visit::visit_expr_return(self, node);
    }

    fn visit_expr_lit(&mut self, node: &'ast ExprLit) {
        let span = node.span();

        match &node.lit {
            Lit::Int(value) => {
                let digits = value.base10_digits();

                if digits != "0" {
                    self.emit("literal.int_to_zero", span, "0", 0);
                }

                if digits != "1" {
                    self.emit("literal.int_to_one", span, "1", 1);
                }

                if let Ok(parsed) = digits.parse::<i64>() {
                    // Checked, because the literal may be `i64::MAX`: wrapping would offer
                    // `i64::MIN` as an "increment", and the unchecked form panics in a debug build.
                    if let Some(incremented) = parsed.checked_add(1) {
                        self.emit("literal.int_increment", span, incremented.to_string(), 2);
                    }

                    if let Some(decremented) = parsed.checked_sub(1) {
                        self.emit("literal.int_decrement", span, decremented.to_string(), 3);
                    }
                }
            }

            Lit::Bool(value) => {
                self.emit("literal.bool_flip", span, (!value.value).to_string(), 0);
            }

            Lit::Str(value) => {
                let text = value.value();

                if !text.is_empty() {
                    self.emit("literal.str_to_empty", span, "\"\"", 0);
                }

                // Replacing `"xyzzy"` with `"xyzzy"` is the original program.
                if text != XYZZY {
                    self.emit("literal.str_to_xyzzy", span, "\"xyzzy\"", 1);
                }
            }

            _ => {}
        }

        visit::visit_expr_lit(self, node);
    }
}

/// Returns a printable name for a type, used to build the enclosing item path for `impl` blocks.
fn type_name(ty: &Type) -> String {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map_or_else(|| "_".to_owned(), |segment| segment.ident.to_string()),

        Type::Reference(reference) => type_name(&reference.elem),
        _ => "_".to_owned(),
    }
}

/// Returns whether borrowing an expression relies on it being promoted to static storage.
///
/// Promotion is a property of const-evaluability, which cannot be decided from syntax alone. What
/// is decidable is the shape that makes it plausible — an aggregate built only from literals — and
/// that is the shape whose promotion a guard would silently take away.
fn is_promotable(expression: &Expr) -> bool {
    match expression {
        // A path is either a constant, which is promotable, or a local, which is not a temporary
        // in the first place. Neither holds a mutation site, so the answer costs nothing either way.
        Expr::Lit(_) | Expr::Path(_) => true,

        Expr::Array(array) => array.elems.iter().all(is_promotable),
        Expr::Tuple(tuple) => tuple.elems.iter().all(is_promotable),
        Expr::Repeat(repeat) => is_promotable(&repeat.expr),
        Expr::Reference(reference) => is_promotable(&reference.expr),
        Expr::Unary(unary) => matches!(unary.op, UnOp::Neg(_)) && is_promotable(&unary.expr),
        Expr::Paren(paren) => is_promotable(&paren.expr),
        Expr::Group(group) => is_promotable(&group.expr),

        _ => false,
    }
}

/// Returns whether a condition binds a pattern, either directly or as part of a `&&` chain.
///
/// A `let` in condition position is not an expression that can be negated, replaced by a boolean,
/// or have its `&&` turned into `||`: all three are rejected by the parser or leave the bindings
/// the rest of the condition and the body depend on unbound.
fn binds_a_pattern(condition: &Expr) -> bool {
    match condition {
        Expr::Let(_) => true,
        Expr::Binary(binary) if matches!(binary.op, BinOp::And(_)) => {
            binds_a_pattern(&binary.left) || binds_a_pattern(&binary.right)
        }
        Expr::Paren(paren) => binds_a_pattern(&paren.expr),
        Expr::Group(group) => binds_a_pattern(&group.expr),
        _ => false,
    }
}

/// Returns the value of a condition that is written as a boolean literal, seeing through grouping.
///
/// Replacing `if true` with `if true` reproduces the original program, so the mutant can never be
/// killed and would sit in every report as a permanent survivor.
fn boolean_literal(condition: &Expr) -> Option<bool> {
    match condition {
        Expr::Lit(ExprLit { lit: Lit::Bool(value), .. }) => Some(value.value),
        Expr::Paren(paren) => boolean_literal(&paren.expr),
        Expr::Group(group) => boolean_literal(&group.expr),
        _ => None,
    }
}

/// Returns whether an expression is a numeric literal equal to zero, seeing through grouping.
///
/// Zero is its own negation, so dropping the `-` from `-0` leaves the program unchanged. Only the
/// literal is recognised: `-x` where `x` happens to be zero at run time is a real mutant, because
/// the same expression is not zero on another execution.
fn is_zero_literal(expression: &Expr) -> bool {
    match expression {
        Expr::Lit(ExprLit { lit: Lit::Int(value), .. }) => value.base10_digits() == "0",
        Expr::Lit(ExprLit { lit: Lit::Float(value), .. }) => {
            value.base10_parse::<f64>().is_ok_and(|parsed| parsed == 0.0)
        }
        Expr::Paren(paren) => is_zero_literal(&paren.expr),
        Expr::Group(group) => is_zero_literal(&group.expr),
        _ => false,
    }
}

/// Returns whether an expression is worth offering `+ 1` and `- 1` for.
///
/// Without type resolution this cannot be more than a syntactic filter, so it is a list of the
/// shapes that plausibly denote a number rather than an attempt to exclude everything that does
/// not. Two exclusions do real work. A literal is left out because the literal family already
/// perturbs it, and emitting both would pay twice for one question. A reference, a closure, a
/// string and a block are left out because none of them adds to an integer, and every mutant that
/// cannot compile costs a rollback round that finds nothing.
fn is_perturbable(expression: &Expr) -> bool {
    match expression {
        // `self` is the receiver, and a receiver that is a number is rare enough that offering the
        // mutant everywhere else costs more than the few it would find.
        Expr::Path(path) if path.path.is_ident("self") => false,

        // A bare `Marker` or `PhantomData` is a unit struct or a variant, not a number. The
        // screaming case is excluded from the test because that is how constants are spelled, and
        // a constant is one of the most useful things this family has to offer.
        Expr::Path(path) => !path.path.get_ident().is_some_and(|ident| is_type_case(&ident.to_string())),

        // `Vec::new()`, `String::from(..)`, `Box::new(..)`: the type is written at the call site,
        // so there is no need to guess what it returns.
        Expr::Call(call) => !callee_type(&call.func).is_some_and(|name| is_non_numeric_type(&name)),

        // A method whose name says what it returns says it more reliably than any inference here
        // could, and every one of these returns something that does not add.
        Expr::MethodCall(call) => !returns_non_numeric(&call.method.to_string()),

        Expr::Field(_) | Expr::Index(_) | Expr::Cast(_) => true,

        // Arithmetic yields a number; a comparison or a logical operator yields a bool.
        Expr::Binary(binary) => matches!(
            binary.op,
            BinOp::Add(_) | BinOp::Sub(_) | BinOp::Mul(_) | BinOp::Div(_) | BinOp::Rem(_)
        ),

        // `-x` is a number. `!x` is a bool or a bitwise complement, and `*x` may be anything.
        Expr::Unary(unary) => matches!(unary.op, UnOp::Neg(_)),

        Expr::Paren(paren) => is_perturbable(&paren.expr),
        Expr::Group(group) => is_perturbable(&group.expr),

        _ => false,
    }
}

/// Returns whether an identifier is spelled the way a type is rather than the way a value is.
///
/// `PhantomData` and `Ordering` are types; `MAX` and `DEFAULT_SIZE` are constants, which are among
/// the most worthwhile things to perturb, so the screaming case has to be told apart from the
/// camel case rather than lumped in with it.
fn is_type_case(name: &str) -> bool {
    name.starts_with(|first: char| first.is_uppercase()) && name.chars().any(char::is_lowercase)
}

/// Returns the type a path call is qualified by: the `Vec` of `Vec::with_capacity`.
fn callee_type(callee: &Expr) -> Option<String> {
    match callee {
        Expr::Path(path) => {
            let mut segments = path.path.segments.iter().rev();
            let _last = segments.next()?;

            segments.next().map(|segment| segment.ident.to_string())
        }

        Expr::Paren(paren) => callee_type(&paren.expr),
        Expr::Group(group) => callee_type(&group.expr),
        _ => None,
    }
}

/// Returns whether a type's associated functions are known never to produce a number.
fn is_non_numeric_type(name: &str) -> bool {
    matches!(
        name,
        "Box" | "Vec" | "String" | "Rc" | "Arc" | "Cow" | "PhantomData" | "HashMap" | "HashSet" | "BTreeMap" | "BTreeSet"
    )
}

/// Returns whether a method's name is enough on its own to know it yields something unaddable.
///
/// Only names whose meaning is fixed across the ecosystem are listed. `clone` is deliberately
/// absent: cloning a number yields a number, and a rule that reads well but is wrong costs a real
/// mutant every time it fires.
fn returns_non_numeric(method: &str) -> bool {
    matches!(
        method,
        "iter"
            | "iter_mut"
            | "into_iter"
            | "collect"
            | "to_string"
            | "to_owned"
            | "to_vec"
            | "as_str"
            | "as_bytes"
            | "as_slice"
            | "as_ref"
            | "as_mut"
            | "chars"
            | "bytes"
            | "keys"
            | "values"
            | "lines"
            | "split"
            | "split_whitespace"
            | "lock"
            | "borrow"
            | "borrow_mut"
            | "to_path_buf"
    )
}

/// Returns whether a callee's arguments describe how much room to set aside rather than what the
/// program should do.
///
/// Perturbing one of these produces a mutant that changes only an allocation strategy. A test
/// suite that caught it would be a test suite pinning an implementation detail, so reporting it as
/// a survivor accuses the tests of a gap they should not be asked to fill.
fn is_capacity_call(name: &str) -> bool {
    matches!(
        name,
        "with_capacity"
            | "with_capacity_in"
            | "reserve"
            | "reserve_exact"
            | "try_reserve"
            | "try_reserve_exact"
            | "shrink_to"
    )
}

/// Returns whether an expression is already `Default::default()`.
///
/// Replacing it with itself would be a mutant no test could ever detect, which would be reported
/// as a survivor and read as an accusation against the suite for something it cannot do.
fn is_default_call(expression: &Expr) -> bool {
    match expression {
        Expr::Call(call) => matches!(callee_name(&call.func).as_deref(), Some("default")),
        Expr::Paren(paren) => is_default_call(&paren.expr),
        Expr::Group(group) => is_default_call(&group.expr),
        _ => false,
    }
}

/// The curated renames for a standard-library method, keyed by name and argument count.
///
/// The argument count is not decoration. Without type resolution the only evidence that a `take`
/// is `Iterator::take` rather than `Option::take` or `Cell::take` is that it was given a count,
/// and swapping the second kind for `skip` would be applying a transformation nobody advertised.
/// Every entry here is a pair of standard-library methods with the same receiver, arity and
/// result *type*, which is a stricter requirement than it sounds. `take` and `skip` ask a genuine
/// question about a chain, but `Take<I>` and `Skip<I>` are different types, and a mutant shares an
/// `if` with the code it replaces, so that swap could never compile. The same rules out
/// `take_while`/`skip_while`. What is left are the methods whose two spellings agree on a type:
/// `bool`, `Option<T>`, `String` and `&str`.
fn method_renames(method: &str, arity: usize) -> Option<&'static [(&'static str, &'static str)]> {
    let swaps: &'static [(&'static str, &'static str)] = match (method, arity) {
        ("any", 1) => &[("iter.any_to_all", "all")],
        ("all", 1) => &[("iter.all_to_any", "any")],

        // Zero arguments is `Iterator::min`; one is `Ord::min`. Both are a choice between the
        // extremes, so both are worth swapping.
        ("min", 0 | 1) => &[("iter.min_to_max", "max")],
        ("max", 0 | 1) => &[("iter.max_to_min", "min")],

        ("first", 0) => &[("iter.first_to_last", "last")],
        ("last", 0) => &[("iter.last_to_first", "first")],

        ("starts_with", 1) => &[("string.starts_with_to_ends_with", "ends_with")],
        ("ends_with", 1) => &[("string.ends_with_to_starts_with", "starts_with")],

        ("to_lowercase", 0) => &[("string.lower_to_upper", "to_uppercase")],
        ("to_uppercase", 0) => &[("string.upper_to_lower", "to_lowercase")],
        ("to_ascii_lowercase", 0) => &[("string.lower_to_upper", "to_ascii_uppercase")],
        ("to_ascii_uppercase", 0) => &[("string.upper_to_lower", "to_ascii_lowercase")],

        ("trim_start", 0) => &[("string.trim_start_to_trim_end", "trim_end")],
        ("trim_end", 0) => &[("string.trim_end_to_trim_start", "trim_start")],

        _ => return None,
    };

    Some(swaps)
}

/// The mutator for deleting an in-place ordering or deduplication call.
///
/// These are the counterpart to the adapters above: because they return `()`, the only way to
/// remove one is to delete the whole statement, and the question they ask — does anything observe
/// that this collection was ordered? — is worth asking under its own name rather than folding it
/// into generic statement deletion.
fn in_place_reorder(expression: &Expr) -> Option<&'static str> {
    let Expr::MethodCall(call) = expression else {
        return None;
    };

    match call.method.to_string().as_str() {
        "sort" | "sort_by" | "sort_by_key" | "sort_unstable" | "sort_unstable_by" | "sort_unstable_by_key" => {
            Some("iter.remove_sort")
        }
        "dedup" | "dedup_by" | "dedup_by_key" => Some("iter.remove_dedup"),
        _ => None,
    }
}

/// Returns whether an expression is a call to one of those functions.
///
/// Their arguments are excluded because perturbing them says nothing; their *results* are excluded
/// because a call that reserves room returns the collection, never a number.
fn is_capacity_result(expression: &Expr) -> bool {
    match expression {
        Expr::Call(call) => callee_name(&call.func).is_some_and(|name| is_capacity_call(&name)),
        Expr::MethodCall(call) => is_capacity_call(&call.method.to_string()),
        Expr::Paren(paren) => is_capacity_result(&paren.expr),
        Expr::Group(group) => is_capacity_result(&group.expr),
        _ => false,
    }
}

/// Returns whether a signature promises a number, which is the only thing worth perturbing by one.
fn is_numeric_return(output: &ReturnType) -> bool {
    let ReturnType::Type(_arrow, ty) = output else {
        return false;
    };

    matches!(resolve_type(ty), Kind::Signed | Kind::Unsigned | Kind::Float)
}

/// Returns whether a type is one no concrete `Default` can be assumed for.
///
/// `Default::default()` is what the `fn_value` family falls back on when it cannot name a value of
/// a type, and the fallback is deliberately optimistic: a concrete type this tool has never heard
/// of very often does have a `Default`, and withholding the mutant would leave the question of
/// whether the returned value is tested unasked. An abstract type is where that optimism has
/// nothing behind it — a caller's `E`, a trait's `Self::Value`, a serializer's `S::Ok` are chosen
/// by whoever implements the trait, and a bound saying they implement `Default` would be written
/// in the signature if it held.
///
/// Three shapes qualify: a bare type parameter with no `Default` bound; an associated type
/// projected out of one, such as `D::Error`, which the caller picks and is equally unconstrained;
/// and a trait object or `impl Trait`, which names a capability rather than a type and so has no
/// `default()` to call at all.
///
/// `Self::Value` is deliberately not among them, even though it looks the same. Inside an `impl`
/// block it resolves to a type that block chose and often does have a `Default` — six mutants that
/// a real suite caught were lost to an earlier version of this rule that treated it as abstract.
fn is_abstract_type(ty: &Type, abstracts: &[String]) -> bool {
    match ty {
        Type::TraitObject(_) | Type::ImplTrait(_) => true,

        Type::Paren(paren) => is_abstract_type(&paren.elem, abstracts),
        Type::Group(group) => is_abstract_type(&group.elem, abstracts),

        Type::Path(path) if path.qself.is_none() => {
            let Some(last) = path.path.segments.last() else {
                return false;
            };

            if path.path.segments.len() > 1 {
                return abstracts.contains(&path.path.segments[0].ident.to_string());
            }

            // `Box<dyn Reader>` is exactly as unconstructable as the `dyn Reader` inside it.
            if last.ident == "Box" {
                return payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, abstracts));
            }

            abstracts.contains(&last.ident.to_string())
        }

        _ => false,
    }
}

/// Names every type parameter a generics list declares without a `Default` bound.
///
/// A parameter written `T: Default` is excluded, because there the promise this is looking for was
/// made explicitly and the mutant it would otherwise withhold compiles.
fn undefaulted_parameters(generics: &Generics) -> Vec<String> {
    generics
        .params
        .iter()
        .filter_map(|param| match param {
            GenericParam::Type(ty) => {
                let defaulted = ty.bounds.iter().any(|bound| {
                    matches!(bound, TypeParamBound::Trait(tr)
                        if tr.path.segments.last().is_some_and(|segment| segment.ident == "Default"))
                });

                (!defaulted).then(|| ty.ident.to_string())
            }

            _ => None,
        })
        .collect()
}

/// Returns a type's `index`th generic argument, so `Result<T, E>` can be asked for either side.
fn payload(ty: &Type, index: usize) -> Option<&Type> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };

    let PathArguments::AngleBracketed(args) = &path.path.segments.last()?.arguments else {
        return None;
    };

    args.args.iter().filter_map(|arg| match arg {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    }).nth(index)
}

/// Returns whether a written-down type is one that `+ 1` applies to.
///
/// References are peeled first: `&usize + 1` compiles, so treating a reference as a non-number
/// would throw away a mutant that builds and runs.
fn is_numeric_binding(ty: &Type) -> bool {
    match ty {
        Type::Reference(reference) => is_numeric_binding(&reference.elem),
        Type::Paren(paren) => is_numeric_binding(&paren.elem),
        Type::Group(group) => is_numeric_binding(&group.elem),

        _ => matches!(resolve_type(ty), Kind::Signed | Kind::Unsigned | Kind::Float),
    }
}

/// Returns the final path segment of a called expression, which is the function's own name.
///
/// `Vec::with_capacity` and a bare `with_capacity` should be recognised as the same thing, and the
/// path in between says nothing the skip list needs.
fn callee_name(callee: &Expr) -> Option<String> {
    match callee {
        Expr::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
        Expr::Paren(paren) => callee_name(&paren.expr),
        Expr::Group(group) => callee_name(&group.expr),
        _ => None,
    }
}

/// Returns whether an operator is a compound assignment, whose left side is a place expression.
const fn is_assign_op(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::AddAssign(_)
            | BinOp::SubAssign(_)
            | BinOp::MulAssign(_)
            | BinOp::DivAssign(_)
            | BinOp::RemAssign(_)
            | BinOp::BitAndAssign(_)
            | BinOp::BitOrAssign(_)
            | BinOp::BitXorAssign(_)
            | BinOp::ShlAssign(_)
            | BinOp::ShrAssign(_)
    )
}

/// Returns whether a function's return type is syntactically a `Result`.
fn returns_result(output: &ReturnType) -> bool {
    matches!(output, ReturnType::Type(_, ty) if resolve_type(ty) == Kind::Result)
}

/// Returns the plausible replacement values for a function's return type.
///
/// The type is read syntactically, so an alias, a generic parameter or an associated type falls
/// through to `Default::default()`, which may not compile — an acceptable trade, since a bad
/// guess just costs one rollback round rather than losing the mutant entirely.
/// How deep the recursion through nested return types is allowed to go.
///
/// A tuple of options of results nests as far as the author cared to write, and each level
/// multiplies the number of values below it. Three levels reaches `Result<Option<bool>, E>`, which
/// is the shape this exists for, and stops well before a type whose values would dominate the
/// population of the whole file.
const RETURN_DEPTH: usize = 3;

/// The most replacement values any single return type may contribute.
///
/// The bound is on the product, not on any one level, because it is the product that decides how
/// many mutants a function costs. A tuple of four booleans is sixteen combinations, and every one
/// of them is a separate build round's worth of test time.
const RETURN_WIDTH: usize = 8;

/// The replacement values worth trying for a function's return type.
///
/// Each entry is a mutator name and the text of a value of that type. The list is generated rather
/// than looked up so that nested types compose: a `Result<Option<bool>, E>` is a `Result` whose
/// success values are the `Option` values, which are in turn the `bool` values.
fn return_values(output: &ReturnType, abstracts: &[String]) -> Vec<(&'static str, String)> {
    let ReturnType::Type(_arrow, ty) = output else {
        return vec![("fn_value.unit", "()".to_owned())];
    };

    values_for(ty, RETURN_DEPTH, abstracts)
}

/// The replacement values for one type, recursing through its parameters.
///
/// `depth` bounds the recursion; at zero the type contributes `Default::default()` rather than
/// nothing, because a value that type-checks is still worth trying even when its shape is unknown.
fn values_for(ty: &Type, depth: usize, abstracts: &[String]) -> Vec<(&'static str, String)> {
    // An abstract type contributes nothing rather than a guess. `Default::default()` is what this
    // family reaches for when it cannot name a value, and for a caller's type parameter or a
    // trait's associated type nothing promises there is one to reach for.
    if is_abstract_type(ty, abstracts) {
        return Vec::new();
    }

    if depth == 0 {
        return vec![("fn_value.default", "Default::default()".to_owned())];
    }

    match resolve_type(ty) {
        Kind::Unit => vec![("fn_value.unit", "()".to_owned())],

        Kind::Bool => vec![
            ("fn_value.bool_true", "true".to_owned()),
            ("fn_value.bool_false", "false".to_owned()),
        ],

        Kind::Signed => vec![
            ("fn_value.zero", "0".to_owned()),
            ("fn_value.one", "1".to_owned()),
            ("fn_value.minus_one", "-1".to_owned()),
        ],

        Kind::Unsigned => vec![("fn_value.zero", "0".to_owned()), ("fn_value.one", "1".to_owned())],

        Kind::Float => vec![
            ("fn_value.zero", "0.0".to_owned()),
            ("fn_value.one", "1.0".to_owned()),
            ("fn_value.minus_one", "-1.0".to_owned()),
        ],

        Kind::StaticStr => vec![
            ("fn_value.empty_string", "\"\"".to_owned()),
            ("fn_value.xyzzy_string", "\"xyzzy\"".to_owned()),
        ],

        Kind::String => vec![
            ("fn_value.empty_string", "String::new()".to_owned()),
            ("fn_value.xyzzy_string", "\"xyzzy\".to_owned()".to_owned()),
        ],

        // A `NonZero` cannot hold the zero every other numeric type offers, so the interesting
        // values are the smallest it can hold and one that is merely different.
        Kind::NonZero => vec![
            ("fn_value.one", format!("{}::new(1).unwrap()", type_text(ty))),
            ("fn_value.two", format!("{}::new(2).unwrap()", type_text(ty))),
        ],

        // The empty case is universal; the one-element case needs a value to put in it, which is
        // what the recursion supplies.
        Kind::Option => {
            let mut values = vec![("fn_value.none", "None".to_owned())];
            let inner = inner_values(ty, 0, depth, abstracts);

            if inner.is_empty() {
                // Nothing is known about the payload, either because it is a type this file cannot
                // resolve or because it is a reference. `Default::default()` is the one expression
                // that stands for a value of any type it fits, so it is what is left to try. It
                // will not always compile, and that is accepted: the alternative is to emit only
                // `None` and never ask whether the present case is tested at all.
                if !payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, abstracts)) {
                    values.push(("fn_value.some_default", "Some(Default::default())".to_owned()));
                }
            } else {
                values
                    .extend(inner.into_iter().map(|(_name, text)| ("fn_value.some", format!("Some({text})"))));
            }

            cap(values)
        }

        Kind::Result => {
            let inner = inner_values(ty, 0, depth, abstracts);
            let mut values = if inner.is_empty() {
                if payload(ty, 0).is_some_and(|inner| is_abstract_type(inner, abstracts)) {
                    Vec::new()
                } else {
                    vec![("fn_value.ok_default", "Ok(Default::default())".to_owned())]
                }
            } else {
                inner.into_iter().map(|(_name, text)| ("fn_value.ok", format!("Ok({text})"))).collect()
            };

            if !payload(ty, 1).is_some_and(|inner| is_abstract_type(inner, abstracts)) {
                values.push(("fn_value.err_default", "Err(Default::default())".to_owned()));
            }

            cap(values)
        }

        // Every one of these builds from an iterator of its element type, so one construction
        // covers all of them and the element values come from the recursion.
        Kind::Collection => {
            let empty = format!("{}::new()", collection_ctor(ty));
            let mut values = vec![("fn_value.empty_collection", empty)];

            values.extend(inner_values(ty, 0, depth, abstracts).into_iter().map(|(_name, text)| {
                ("fn_value.one_element", format!("core::iter::once({text}).collect()"))
            }));

            cap(values)
        }

        // A map's element is a pair, so its one-element form needs both parameters rather than the
        // first alone.
        Kind::Map => {
            let empty = format!("{}::new()", collection_ctor(ty));
            let mut values = vec![("fn_value.empty_collection", empty)];

            let keys = inner_values(ty, 0, depth, abstracts);
            let vals = inner_values(ty, 1, depth, abstracts);

            if let (Some((_kn, key)), Some((_vn, value))) = (keys.first(), vals.first()) {
                values.push((
                    "fn_value.one_element",
                    format!("core::iter::once(({key}, {value})).collect()"),
                ));
            }

            cap(values)
        }

        // A smart pointer is transparent to the caller's reasoning, so the values worth trying are
        // its contents wrapped back up.
        Kind::Wrapper => {
            let ctor = wrapper_ctor(ty);

            cap(inner_values(ty, 0, depth, abstracts)
                .into_iter()
                .map(|(name, text)| (name, format!("{ctor}({text})")))
                .collect())
        }

        // `Cow` is a wrapper whose constructor is a variant rather than a function, and `Owned`
        // is the variant that does not borrow from anything in scope.
        Kind::Cow => cap(inner_values(ty, 0, depth, abstracts)
            .into_iter()
            .map(|(name, text)| (name, format!("std::borrow::Cow::Owned({text})")))
            .collect()),

        // The two kinds nothing can be offered for, for the same underlying reason: the replacement
        // would not share a type with the code it replaces. An `impl Iterator` return is one
        // concrete type chosen by the body, so `Empty<T>`, `Once<T>` and whatever the author wrote
        // are three different types that cannot be arms of one `if`. A reference implements
        // `Default` only for `&str` and `&[T]`, both classified above, so `Default::default()` is
        // known in advance not to compile. Emitting either would buy a guaranteed-unviable mutant,
        // and every unviable mutant costs a build round to withdraw.
        Kind::Iterator | Kind::Reference => Vec::new(),

        // Every combination of the elements' values, which is where the product bound earns its
        // keep: three fields with three values each is twenty-seven mutants for one function.
        Kind::Tuple => tuple_values(ty, depth, abstracts),

        Kind::Unknown => vec![("fn_value.default", "Default::default()".to_owned())],
    }
}

/// Every combination of a tuple's element values, which is where the width bound earns its keep:
/// three fields with three values each is twenty-seven mutants for a single function, and the
/// user has to read every one of them.
fn tuple_values(ty: &Type, depth: usize, abstracts: &[String]) -> Vec<(&'static str, String)> {
    let Type::Tuple(tuple) = strip(ty) else {
        return vec![("fn_value.default", "Default::default()".to_owned())];
    };

    let mut combinations: Vec<Vec<String>> = vec![Vec::new()];

    for element in &tuple.elems {
        let choices = values_for(element, depth.saturating_sub(1), abstracts);
        let mut next = Vec::new();

        for existing in &combinations {
            for (_name, text) in &choices {
                if next.len() >= RETURN_WIDTH {
                    break;
                }

                let mut combination = existing.clone();

                combination.push(text.clone());
                next.push(combination);
            }
        }

        combinations = next;
    }

    combinations.into_iter().map(|parts| ("fn_value.tuple", format!("({})", parts.join(", ")))).collect()
}

/// Truncates a value list to the width bound.
fn cap(mut values: Vec<(&'static str, String)>) -> Vec<(&'static str, String)> {
    values.truncate(RETURN_WIDTH);
    values
}

/// The values of a generic type's `index`th type parameter.
///
/// Lifetime and const parameters are skipped, so `Cow<'a, str>` finds `str` at index zero the way
/// `Option<T>` finds `T`.
fn inner_values(ty: &Type, index: usize, depth: usize, abstracts: &[String]) -> Vec<(&'static str, String)> {
    type_argument(ty, index).map_or_else(
        || vec![("fn_value.default", "Default::default()".to_owned())],
        |inner| values_for(inner, depth.saturating_sub(1), abstracts),
    )
}

/// The `index`th type argument of a path type, ignoring lifetimes and const generics.
fn type_argument(ty: &Type, index: usize) -> Option<&Type> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };

    let PathArguments::AngleBracketed(args) = &path.path.segments.last()?.arguments else {
        return None;
    };

    args.args
        .iter()
        .filter_map(|arg| match arg {
            GenericArgument::Type(inner) => Some(inner),
            _ => None,
        })
        .nth(index)
}

/// The path text of a type, so that an associated function can be called on it.
fn type_text(ty: &Type) -> String {
    match strip(ty) {
        Type::Path(path) => path
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::"),
        _ => "Default".to_owned(),
    }
}

/// The constructor path for a collection type, keeping any qualification the author wrote.
fn collection_ctor(ty: &Type) -> String {
    let Type::Path(path) = strip(ty) else {
        return "Vec".to_owned();
    };

    path.path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The constructor for a smart-pointer type.
fn wrapper_ctor(ty: &Type) -> String {
    format!("{}::new", collection_ctor(ty))
}

/// Sees through parentheses and groups to the type underneath.
fn strip(ty: &Type) -> &Type {
    match ty {
        Type::Paren(paren) => strip(&paren.elem),
        Type::Group(group) => strip(&group.elem),
        other => other,
    }
}

/// The coarse classification of a return type that decides which values are worth trying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Unit,
    Bool,
    Signed,
    Unsigned,
    Float,
    StaticStr,
    String,
    NonZero,
    Option,
    Result,
    /// Anything built from an iterator of a single element type: `Vec`, `VecDeque`, sets, heaps.
    Collection,
    /// Anything built from an iterator of key-value pairs.
    Map,
    /// A smart pointer constructed by `new`: `Box`, `Rc`, `Arc`.
    Wrapper,
    Cow,
    Iterator,
    Tuple,
    Reference,
    Unknown,
}

/// Classifies a return type syntactically.
fn resolve_type(ty: &Type) -> Kind {
    match ty {
        Type::Tuple(tuple) if tuple.elems.is_empty() => Kind::Unit,

        Type::Reference(reference) => match &*reference.elem {
            Type::Path(path) if path.path.is_ident("str") => Kind::StaticStr,

            // `&[T]` has a `Default`, unlike references in general, so it keeps the mutant that
            // depends on one.
            Type::Slice(_) => Kind::Unknown,

            _ => Kind::Reference,
        },

        Type::Tuple(_) => Kind::Tuple,

        Type::Paren(paren) => resolve_type(&paren.elem),
        Type::Group(group) => resolve_type(&group.elem),

        // Only `impl Iterator` is recognised. Any other `impl Trait` has no expression this tool
        // can name that is guaranteed to satisfy it.
        Type::ImplTrait(imp) => {
            let iterator = imp.bounds.iter().any(|bound| {
                matches!(bound, TypeParamBound::Trait(tr)
                    if tr.path.segments.last().is_some_and(|segment| segment.ident == "Iterator"))
            });

            if iterator { Kind::Iterator } else { Kind::Unknown }
        }

        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return Kind::Unknown;
            };

            let name = segment.ident.to_string();

            if name.starts_with("NonZero") && name != "NonZero" {
                return Kind::NonZero;
            }

            match name.as_str() {
                "bool" => Kind::Bool,
                "i8" | "i16" | "i32" | "i64" | "i128" | "isize" => Kind::Signed,
                "u8" | "u16" | "u32" | "u64" | "u128" | "usize" => Kind::Unsigned,
                "f32" | "f64" => Kind::Float,
                "String" => Kind::String,
                "Option" => Kind::Option,
                "Result" => Kind::Result,
                "Vec" | "VecDeque" | "HashSet" | "BTreeSet" | "BinaryHeap" | "LinkedList" => Kind::Collection,
                "HashMap" | "BTreeMap" => Kind::Map,
                "Box" | "Rc" | "Arc" => Kind::Wrapper,
                "Cow" => Kind::Cow,
                _ => Kind::Unknown,
            }
        }

        _ => Kind::Unknown,
    }
}

/// The mutators and replacement operators available for a binary operator.
const fn binary_replacements(op: &BinOp) -> &'static [(&'static str, &'static str)] {
    match op {
        BinOp::Lt(_) => &[("relational.lt_to_le", "<="), ("relational.lt_to_gt", ">")],
        BinOp::Le(_) => &[("relational.le_to_lt", "<"), ("relational.le_to_ge", ">=")],
        BinOp::Gt(_) => &[("relational.gt_to_ge", ">="), ("relational.gt_to_lt", "<")],
        BinOp::Ge(_) => &[("relational.ge_to_gt", ">"), ("relational.ge_to_le", "<=")],
        BinOp::Eq(_) => &[("relational.eq_to_ne", "!=")],
        BinOp::Ne(_) => &[("relational.ne_to_eq", "==")],

        BinOp::Add(_) => &[("arith.add_to_sub", "-"), ("arith.add_to_mul", "*")],
        BinOp::Sub(_) => &[("arith.sub_to_add", "+"), ("arith.sub_to_div", "/")],
        BinOp::Mul(_) => &[("arith.mul_to_div", "/"), ("arith.mul_to_add", "+")],
        BinOp::Div(_) => &[("arith.div_to_mul", "*"), ("arith.div_to_rem", "%")],
        BinOp::Rem(_) => &[("arith.rem_to_div", "/"), ("arith.rem_to_mul", "*")],

        BinOp::BitAnd(_) => &[("bitwise.and_to_or", "|"), ("bitwise.and_to_xor", "^")],
        BinOp::BitOr(_) => &[("bitwise.or_to_and", "&")],
        BinOp::BitXor(_) => &[("bitwise.xor_to_and", "&")],
        BinOp::Shl(_) => &[("shift.shl_to_shr", ">>")],
        BinOp::Shr(_) => &[("shift.shr_to_shl", "<<")],

        BinOp::And(_) => &[("logical.and_to_or", "||")],
        BinOp::Or(_) => &[("logical.or_to_and", "&&")],

        BinOp::AddAssign(_) => &[("assign.add_to_sub", "-=")],
        BinOp::SubAssign(_) => &[("assign.sub_to_add", "+=")],
        BinOp::MulAssign(_) => &[("assign.mul_to_div", "/=")],
        BinOp::DivAssign(_) => &[("assign.div_to_mul", "*=")],
        BinOp::RemAssign(_) => &[("assign.rem_to_div", "/=")],
        BinOp::BitAndAssign(_) => &[("assign.and_to_or", "|=")],
        BinOp::BitOrAssign(_) => &[("assign.or_to_and", "&=")],
        BinOp::BitXorAssign(_) => &[("assign.xor_to_and", "&=")],
        BinOp::ShlAssign(_) => &[("assign.shl_to_shr", ">>=")],
        BinOp::ShrAssign(_) => &[("assign.shr_to_shl", "<<=")],

        _ => &[],
    }
}
