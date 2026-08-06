use serde::{Deserialize, Serialize};

/// What kind of construct a mutation site is, which decides how a guard can be wrapped around it.
///
/// The three cases are not stylistic. Rust will not accept the same guard text in all three
/// positions, so the shape has to travel with the mutant all the way to instrumentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
    /// An expression. Guarded by a parenthesized `if`/`else` yielding one of two values.
    ///
    /// The parentheses matter: a bare block or `if` in condition position (`if { .. } { .. }`)
    /// is rejected, and without them the guard would also rebind precedence against whatever
    /// operator encloses the site.
    Expr,

    /// A block that must stay a block, such as a function body. Guarded by an `if`/`else` whose
    /// `else` arm is the original block, wrapped in braces so the result is still a block.
    Block,

    /// A whole statement, which the mutant deletes. Guarded by a negated `if` that runs the
    /// original only when the mutant is inactive.
    Stmt,

    /// A match arm's pattern, which the mutant stops from matching.
    ///
    /// Deleting an arm outright is not something a runtime guard can do, because which arms exist
    /// is fixed when the code is compiled. Adding a guard achieves the same behaviour: an arm
    /// whose guard is false does not match, and control falls through to whatever follows. This is
    /// why the collector only offers the mutant when a later wildcard arm is there to catch it.
    ///
    /// It also costs a constant amount of text. The obvious alternative — replacing the whole
    /// `match` with a copy that lacks the arm — grows with the square of the arm count, so a
    /// hundred-arm dispatch would emit a hundred copies of itself and price the family out of any
    /// codebase large enough to want it.
    Arm,
}
