# Profiles

A profile is a named set of mutators. It exists so that a question you ask often — *"is my
arithmetic tested?"*, *"what would cargo-mutants have found?"* — is one word on the command line
rather than a list you have to keep in your head and keep up to date as the catalog grows.

Profiles are written with a leading `@` and are accepted anywhere a mutator name is: `--ops`, the
`ops` key in `gamma.toml`, and every [suppression](SUPPRESSION.md) directive. They compose with
families, individual mutators and `!` negation, applied left to right:

```bash
cargo gamma run --ops @arithmetic            # just the number crunching
cargo gamma run --ops @control,@logical      # two profiles at once
cargo gamma run --ops @all,!stmt             # everything but statement deletion
cargo gamma run --ops @numeric,!literal.int  # a profile, less one mutator
```

`cargo gamma list profiles` prints the table below resolved against your current configuration.

<!-- begin generated: profiles -->

| Profile | What it selects | Expands to |
| --- | --- | --- |
| `@all` | every registered mutator | `*` |
| `@default` | the mutators enabled when none are named, which is currently all of them | `@default` |
| `@parity` | the cargo-mutants operator set, for the differential oracle | `fn_value` |
| `@boundary` | relational and boundary conditions | `relational`, `range` |
| `@arithmetic` | arithmetic, bitwise, shift and compound assignment | `arith`, `bitwise`, `shift`, `assign` |
| `@logical` | logical operators and branch conditions | `logical`, `cond`, `match_guard` |
| `@control` | the choices control flow makes: conditions, guards, arms and loop exits | `cond`, `match_guard`, `match_arm`, `loop` |
| `@removal` | statement and side-effect deletion | `stmt`, `unary`, `match_arm`, `struct_field`, `collection` |
| `@semantics` | standard-library meaning: Option, Result, iterators, strings and collections | `option`, `result`, `iter`, `string`, `collection`, `assign_value` |
| `@literals` | literal and constant replacement | `literal` |
| `@numeric` | literal replacement and focused numeric expression perturbation | `literal`, `expr` |
| `@extreme` | a synonym for `all`, kept because scripts name it | `*` |

<!-- end generated -->

## Which one to reach for

**`@parity` is where to start if you are coming from cargo-mutants.** It is exactly `fn_value`:
replacing what a function returns, and nothing else. That is the whole of what cargo-mutants
generates, so a `@parity` run is directly comparable to one of its runs, with none of the extra
population this tool would otherwise produce. Once you trust the numbers, drop the flag and let the
rest of the catalog run.

**`@default` is what you get with no `--ops` at all.** Today that is the entire catalog. It is named
so that a script can say what it means, and so that `@default,!literal` reads as an adjustment to
the shipped policy rather than a list that has to be re-derived every release.

**`@extreme` is a second spelling of `@all`.** It selects the entire catalog, and it is kept only
because scripts written against earlier versions name it. New configuration should say `@all`.

**`@boundary` is the highest-yield profile per mutant.** Off-by-one errors are the defect class
mutation testing is best at exposing, and a surviving `relational` or `range` mutant almost always
names a real missing assertion rather than an equivalent program.

A profile is a starting point, not a commitment. If a profile is close but not right, name it and
subtract: `--ops @semantics,!option` keeps the shape of the profile and records the one deviation in
a form a reviewer can read.
