# Mutation operators

Every mutator has one stable name of the form `family.transform`. That single name is the whole
vocabulary: it is what `--ops` selects, what a suppression directive names, what the report prints
in brackets after each mutant, what a SARIF rule identifier is set to, and what `explain` accepts.
Nothing refers to a mutator by number or by position, so a name you write down today keeps working
as the catalog grows.

* [Choosing what to run](#choosing-what-to-run)
* [The families](#the-families)
* [Every operator](#every-operator)
* [What the catalog deliberately omits](#what-the-catalog-deliberately-omits)

Related: [profiles](PROFILES.md) group these into named sets, and [suppression](SUPPRESSION.md)
covers how to turn one off for a particular site.

## Choosing what to run

Every mutator is on by default. A mutator that needs a flag before it will ever run is one nobody
runs, and a gap in a score that nobody can see is worse than a mutant somebody has to spend a minute
judging.

A selector is a mutator name, a family prefix, a [profile](PROFILES.md), or an academic alias. `!`
removes from the set, and selectors apply left to right:

```bash
cargo gamma run --ops relational              # one family
cargo gamma run --ops relational.lt_to_le     # one mutator
cargo gamma run --ops @arithmetic,!bitwise    # a profile, less one family
cargo gamma run --ops all,!stmt               # everything except one family
cargo gamma run --ops ROR                     # by academic alias
```

A selector that matches nothing is an error rather than a silent no-op. A filter that quietly does
nothing leaves the score high and gives nobody a reason to look.

To see the catalog as your current selection resolves it, with a `*` against each enabled mutator:

```bash
cargo gamma list ops
cargo gamma explain relational.lt_to_le   # what one does, and how to switch it off
```

## The families

<!-- begin generated: families -->

| Family | Mutators | What it asks |
| --- | ---: | --- |
| [`fn_value`](OPERATORS.md#fn_value) | 20 | Does anything check what this function returns? |
| [`relational`](OPERATORS.md#relational) | 10 | Is this comparison's boundary the right one? |
| [`arith`](OPERATORS.md#arith) | 10 | Does this calculation's operator matter? |
| [`bitwise`](OPERATORS.md#bitwise) | 4 | Is this mask or flag combination correct? |
| [`shift`](OPERATORS.md#shift) | 2 | Is this shift's direction load-bearing? |
| [`assign`](OPERATORS.md#assign) | 10 | Does this compound assignment's operator matter? |
| [`logical`](OPERATORS.md#logical) | 2 | Is this `&&` really an `&&`? |
| [`cond`](OPERATORS.md#cond) | 3 | Does anything depend on this branch being taken? |
| [`match_guard`](OPERATORS.md#match_guard) | 3 | Does anything depend on this guard being right? |
| [`match_arm`](OPERATORS.md#match_arm) | 1 | Is this arm reachable, and does anything notice when it stops matching? |
| [`struct_field`](OPERATORS.md#struct_field) | 1 | Does this field's value matter, or is the default good enough? |
| [`range`](OPERATORS.md#range) | 2 | Is this bound inclusive on purpose? |
| [`loop`](OPERATORS.md#loop) | 4 | Does this `break` or `continue` carry the loop's meaning? |
| [`unary`](OPERATORS.md#unary) | 2 | Does this negation or complement matter? |
| [`literal`](OPERATORS.md#literal) | 7 | Does this constant's exact value matter? |
| [`stmt`](OPERATORS.md#stmt) | 2 | Does this statement's side effect matter? |
| [`expr`](OPERATORS.md#expr) | 2 | Would an off-by-one here be caught? |
| [`option`](OPERATORS.md#option) | 2 | Is the present case distinguished from the absent one? |
| [`result`](OPERATORS.md#result) | 2 | Is success distinguished from failure? |
| [`iter`](OPERATORS.md#iter) | 8 | Does anything observe that this was ordered, deduplicated, or taken from one end? |
| [`string`](OPERATORS.md#string) | 6 | Does the prefix, the case, or the trimmed end actually matter? |
| [`collection`](OPERATORS.md#collection) | 1 | Does every element of this literal earn its place? |
| [`assign_value`](OPERATORS.md#assign_value) | 1 | Is the value assigned here ever read in a way that would notice? |
| **Total** | **105** | |

<!-- end generated -->

## Every operator

The `Alias` column gives the academic name where the operator has one, so a mutation-testing
paper's terminology selects the same thing this tool calls something else. `Default` records whether
the mutator runs when `--ops` is not given, which is currently true of all of them.

<!-- begin generated: operators -->

### `fn_value`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `fn_value.default` | replace the function body with a default value | `RV` | yes |
| `fn_value.unit` | replace the body of a unit function with () |  | yes |
| `fn_value.bool_true` | replace the body with true |  | yes |
| `fn_value.bool_false` | replace the body with false |  | yes |
| `fn_value.zero` | replace the body with 0 |  | yes |
| `fn_value.one` | replace the body with 1 |  | yes |
| `fn_value.minus_one` | replace the body with -1 |  | yes |
| `fn_value.empty_string` | replace the body with an empty string |  | yes |
| `fn_value.xyzzy_string` | replace the body with a non-empty string |  | yes |
| `fn_value.none` | replace the body with None |  | yes |
| `fn_value.some_default` | replace the body with Some(Default::default()) |  | yes |
| `fn_value.ok_default` | replace the body with Ok(Default::default()) |  | yes |
| `fn_value.err_default` | replace the body with Err(Default::default()) |  | yes |
| `fn_value.err_with` | replace the body with Err(v) for each --error value |  | yes |
| `fn_value.two` | replace the body with 2 |  | yes |
| `fn_value.some` | replace the body with Some(value) |  | yes |
| `fn_value.ok` | replace the body with Ok(value) |  | yes |
| `fn_value.empty_collection` | replace the body with an empty collection or iterator |  | yes |
| `fn_value.one_element` | replace the body with a one-element collection or iterator |  | yes |
| `fn_value.tuple` | replace the body with a tuple of replacement values |  | yes |

### `relational`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `relational.lt_to_le` | replace < with <= | `ROR` | yes |
| `relational.lt_to_gt` | replace < with > | `ROR` | yes |
| `relational.le_to_lt` | replace <= with < | `ROR` | yes |
| `relational.le_to_ge` | replace <= with >= | `ROR` | yes |
| `relational.gt_to_ge` | replace > with >= | `ROR` | yes |
| `relational.gt_to_lt` | replace > with < | `ROR` | yes |
| `relational.ge_to_gt` | replace >= with > | `ROR` | yes |
| `relational.ge_to_le` | replace >= with <= | `ROR` | yes |
| `relational.eq_to_ne` | replace == with != | `ROR` | yes |
| `relational.ne_to_eq` | replace != with == | `ROR` | yes |

### `arith`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `arith.add_to_sub` | replace + with - | `AOR` | yes |
| `arith.add_to_mul` | replace + with * | `AOR` | yes |
| `arith.sub_to_add` | replace - with + | `AOR` | yes |
| `arith.sub_to_div` | replace - with / | `AOR` | yes |
| `arith.mul_to_div` | replace * with / | `AOR` | yes |
| `arith.mul_to_add` | replace * with + | `AOR` | yes |
| `arith.div_to_mul` | replace / with * | `AOR` | yes |
| `arith.div_to_rem` | replace / with % | `AOR` | yes |
| `arith.rem_to_div` | replace % with / | `AOR` | yes |
| `arith.rem_to_mul` | replace % with * | `AOR` | yes |

### `bitwise`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `bitwise.and_to_or` | replace & with \| | `AOR` | yes |
| `bitwise.or_to_and` | replace \| with & | `AOR` | yes |
| `bitwise.xor_to_and` | replace ^ with & | `AOR` | yes |
| `bitwise.and_to_xor` | replace & with ^ | `AOR` | yes |

### `shift`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `shift.shl_to_shr` | replace << with >> | `AOR` | yes |
| `shift.shr_to_shl` | replace >> with << | `AOR` | yes |

### `assign`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `assign.add_to_sub` | replace += with -= | `ASR` | yes |
| `assign.sub_to_add` | replace -= with += | `ASR` | yes |
| `assign.mul_to_div` | replace *= with /= | `ASR` | yes |
| `assign.div_to_mul` | replace /= with *= | `ASR` | yes |
| `assign.rem_to_div` | replace %= with /= | `ASR` | yes |
| `assign.and_to_or` | replace &= with \|= | `ASR` | yes |
| `assign.or_to_and` | replace \|= with &= | `ASR` | yes |
| `assign.xor_to_and` | replace ^= with &= | `ASR` | yes |
| `assign.shl_to_shr` | replace <<= with >>= | `ASR` | yes |
| `assign.shr_to_shl` | replace >>= with <<= | `ASR` | yes |

### `logical`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `logical.and_to_or` | replace && with \|\| | `LCR` | yes |
| `logical.or_to_and` | replace \|\| with && | `LCR` | yes |

### `cond`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `cond.negate` | negate a branch condition | `COR` | yes |
| `cond.always_true` | force a branch condition to true | `COR` | yes |
| `cond.always_false` | force a branch condition to false | `COR` | yes |

### `match_guard`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `match_guard.negate` | negate a match arm's guard | `COR` | yes |
| `match_guard.always_true` | force a match arm's guard to true | `COR` | yes |
| `match_guard.always_false` | force a match arm's guard to false | `COR` | yes |

### `match_arm`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `match_arm.never_matches` | stop a match arm from matching, falling through to the wildcard | `SDL` | yes |

### `struct_field`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `struct_field.omit` | omit a struct literal field, leaving the base expression to supply it | `SDL` | yes |

### `range`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `range.exclusive_to_inclusive` | extend a .. range to cover its endpoint | `ROR` | yes |
| `range.inclusive_to_exclusive` | shrink a ..= range to stop short of its endpoint | `ROR` | yes |

### `loop`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `loop.break_to_continue` | replace break with continue |  | yes |
| `loop.continue_to_break` | replace continue with break |  | yes |
| `loop.delete_break` | delete a break statement | `SDL` | yes |
| `loop.delete_continue` | delete a continue statement | `SDL` | yes |

### `unary`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `unary.remove_neg` | remove a unary minus | `UOI` | yes |
| `unary.remove_not` | remove a unary not | `UOI` | yes |

### `literal`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `literal.int_to_zero` | replace an integer literal with 0 | `CRP` | yes |
| `literal.int_to_one` | replace an integer literal with 1 | `CRP` | yes |
| `literal.int_increment` | add one to an integer literal | `CRP` | yes |
| `literal.int_decrement` | subtract one from an integer literal | `CRP` | yes |
| `literal.bool_flip` | invert a boolean literal | `CRP` | yes |
| `literal.str_to_empty` | replace a string literal with an empty string | `CRP` | yes |
| `literal.str_to_xyzzy` | replace a string literal with a different string | `CRP` | yes |

### `stmt`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `stmt.delete_call` | delete a statement whose value is discarded | `SDL` | yes |
| `stmt.delete_assign` | delete a compound assignment statement | `SDL` | yes |

### `expr`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `expr.increment` | add one to a numeric expression in a boundary-sensitive position | `EVR` | yes |
| `expr.decrement` | subtract one from a numeric expression in a boundary-sensitive position | `EVR` | yes |

### `option`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `option.some_to_none` | replace Some(value) with None | `EVR` | yes |
| `option.none_to_some` | replace None with Some(Default::default()) | `EVR` | yes |

### `result`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `result.ok_to_err` | replace Ok(value) with Err(Default::default()) | `EVR` | yes |
| `result.err_to_ok` | replace Err(value) with Ok(Default::default()) | `EVR` | yes |

### `iter`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `iter.any_to_all` | replace any with all | `EVR` | yes |
| `iter.all_to_any` | replace all with any | `EVR` | yes |
| `iter.min_to_max` | replace min with max | `EVR` | yes |
| `iter.max_to_min` | replace max with min | `EVR` | yes |
| `iter.first_to_last` | replace first with last | `EVR` | yes |
| `iter.last_to_first` | replace last with first | `EVR` | yes |
| `iter.remove_sort` | remove a sort from a chain | `SDL` | yes |
| `iter.remove_dedup` | remove a deduplication from a chain | `SDL` | yes |

### `string`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `string.starts_with_to_ends_with` | replace starts_with with ends_with | `EVR` | yes |
| `string.ends_with_to_starts_with` | replace ends_with with starts_with | `EVR` | yes |
| `string.lower_to_upper` | replace to_lowercase with to_uppercase | `EVR` | yes |
| `string.upper_to_lower` | replace to_uppercase with to_lowercase | `EVR` | yes |
| `string.trim_start_to_trim_end` | replace trim_start with trim_end | `EVR` | yes |
| `string.trim_end_to_trim_start` | replace trim_end with trim_start | `EVR` | yes |

### `collection`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `collection.omit_element` | omit an element from a vec! literal | `SDL` | yes |

### `assign_value`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `assign_value.default` | replace an assigned value with its type's default | `EVR` | yes |

<!-- end generated -->

## What the catalog deliberately omits

Three constraints shape what the catalog can express, and all three come from one fact: a mutant is
run by wrapping its site as `if guard { mutant } else { original }`, so a mutant must have the same
*type* as the code it replaces.

**`range` moves the endpoint rather than rewriting `..` as `..=`.** The two say the same thing —
`a..b + 1` covers exactly what `a..=b` covers — but `Range` and `RangeInclusive` are different
types, so the literal rewrite could never compile.

**`iter` swaps only methods whose two spellings agree on a type.** `any`/`all` and
`starts_with`/`ends_with` return `bool`; `min`/`max` and `first`/`last` return `Option<T>`. Swapping
`take` for `skip` asks a real question about a chain, but `Take<I>` and `Skip<I>` are different
types, so it is absent — as is dropping a `filter`, which would turn `Filter<I>` back into `I`.
`sort` and `dedup` return `()` and work in place, so they are reached by deleting the statement
instead.

**A function returning `impl Iterator` gets no return-value mutants at all.** An `impl Trait` return
is a single concrete type chosen by the body, so `Empty<T>`, `Once<T>` and whatever the author
actually wrote are three different types that cannot be two arms of one `if`.

### How return values are synthesized

`fn_value` recurses through the return type rather than reaching straight for `Default::default()`.
A `Result<Option<bool>, E>` yields `Err(Default::default())`, `Ok(None)`, `Ok(Some(true))` and
`Ok(Some(false))`, and the same recursion covers tuples, collections, maps, `Box`, `Rc`, `Arc`,
`Cow` and `NonZero`. Depth and width are bounded so a deeply generic signature cannot generate an
unbounded population.

Where the tool cannot name a value of a type it falls back to `Default::default()`, optimistically:
a concrete type it has never heard of usually does have a `Default`. It withholds that guess only
where nothing could support it — a bare type parameter, an associated type projected out of one such
as `D::Error`, an `impl Trait`, or a `Box<dyn Trait>`. A parameter declared `T: Default` keeps its
mutant, because there the promise is explicit.

To reach an error type that has no `Default`, name the values yourself:

```bash
cargo gamma run --error 'MyError::Io' --error 'MyError::Eof'
```

Each becomes its own `fn_value.err_with` mutant on every function returning a `Result`.

### Mutants that cannot compile

Because everything is on, some mutants will not compile — `struct_field.omit` fires on every literal
struct, and `expr` perturbs values it cannot always prove are numeric. These are withdrawn
automatically, in batches rather than one build each, and reported as `unviable` rather than counted
against the score. `--unviable` lists them if you want to see what was discarded.
