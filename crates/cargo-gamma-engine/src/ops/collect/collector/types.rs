//! What the file's own declarations say about the types a function mentions.

use syn::{GenericParam, Generics, ReturnType, Type};

use crate::HashMap;
use crate::ops::collect::Defaults;

use super::super::defaults::{DefaultPaths, standard_defaulted_parameters};
use super::predicates::payload;

use super::values::{Kind, resolve_type, strip};

/// What the value-choosing functions know about the file they are reasoning inside.
///
/// The two facts travel together through the whole recursion and neither is ever used without the
/// other, so they are carried as one thing rather than as a growing parameter list.
pub(super) struct Types<'a> {
    /// Type names in scope that cannot be constructed: type parameters without a `Default` bound,
    /// associated types, and trait objects.
    pub(super) abstracts: &'a [String],

    /// The module path each imported name came from, so a bare type name can be traced to its crate.
    pub(super) imports: &'a HashMap<String, Option<Vec<String>>>,

    /// What the workspace's own sources say about which of their types implement `Default`.
    pub(super) defaults: &'a Defaults,
}

impl Types<'_> {
    /// Returns whether a type has no `Default` to reach for.
    ///
    /// Two independent readings say so, and either is enough: an error type from another crate,
    /// which effectively never has one, or a type this workspace defines and gives none.
    pub(super) fn lacks_default(&self, ty: &Type) -> bool {
        is_foreign_error(ty, self.imports) || self.defaults.lacks_default(ty)
    }
}

/// The one type outside this workspace whose name ends in `Error` and that does implement `Default`.
///
/// `core::fmt::Error` is a unit struct standing for "formatting failed", and it derives everything.
/// Every other error type in `std` that was checked does not implement `Default`, so it is cheaper
/// to name the exception than to enumerate the rule.
pub(super) const DEFAULTABLE_ERROR: &str = "fmt";

/// Returns whether a type is an error type from outside this workspace, which will have no `Default`.
///
/// Error types are the largest single source of mutants that cannot compile: `Err(Default::default())`
/// wants an error value, and `std::io::Error`, `anyhow::Error`, `serde_json::Error` and their kind
/// have no `Default` and are not going to acquire one. Withholding the mutant there costs no signal,
/// because there was never a mutant to lose.
///
/// The rule is deliberately confined to types from *other* crates. A workspace error type may well
/// be an enum with a `#[default]` variant, and the collector cannot see the definition to find out,
/// so `crate::`, `self::`, `super::` and any name this file does not import stay optimistic.
pub(super) fn is_foreign_error(ty: &Type, imports: &HashMap<String, Option<Vec<String>>>) -> bool {
    let Type::Path(path) = strip(ty) else {
        return false;
    };

    let Some(last) = path.path.segments.last() else {
        return false;
    };

    if !last.ident.to_string().ends_with("Error") {
        return false;
    }

    // Written out in full, so where it comes from is right there. A single-segment path is a bare
    // name instead, and only the file's imports can say what it was.
    let owned;
    let prefix: &[String] = if path.path.segments.len() > 1 {
        owned = path
            .path
            .segments
            .iter()
            .rev()
            .skip(1)
            .rev()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        &owned
    } else {
        match imports.get(&last.ident.to_string()) {
            // A name two `use` items disagree about is as unknown as one never imported, so it
            // takes the same answer rather than the last writer's.
            Some(Some(path)) => path,
            Some(None) | None => return false,
        }
    };

    let (Some(root), Some(qualifier)) = (prefix.first(), prefix.last()) else {
        return false;
    };

    if matches!(root.as_str(), "crate" | "self" | "super") {
        return false;
    }

    qualifier != DEFAULTABLE_ERROR
}

/// Returns whether a signature's `Result` fixes its error type to one with no `Default`.
///
/// `Ok(v)` becoming `Err(Default::default())` needs a value of the error type, and the call site
/// does not name it — the signature does. When the error type is written out, it is read straight
/// off the return type. When the signature uses a crate-wide `type Result<T>` alias instead, which
/// is close to universal in real Rust, the alias is resolved through the workspace index.
pub(super) fn returns_undefaultable_error(output: &ReturnType, types: &Types<'_>) -> bool {
    let ReturnType::Type(_arrow, ty) = output else {
        return false;
    };

    if resolve_type(ty) != Kind::Result {
        return false;
    }

    if let Some(error) = payload(ty, 1) {
        return types.lacks_default(error);
    }

    // No second argument, so the error type is whatever an alias fixed it to. The alias is named by
    // the return type's last segment, and the index answers for the name it resolved to.
    let Type::Path(path) = strip(ty) else {
        return false;
    };

    let Some(alias) = path.path.segments.last() else {
        return false;
    };

    types
        .defaults
        .aliased_error(&alias.ident.to_string())
        .is_some_and(|error| types.defaults.lacks_error_default(error))
}

/// Returns whether a type is one no concrete `Default` can be assumed for.
///
/// `Default::default()` is the fallback for an unknown concrete type, but a caller's type
/// parameter, an associated type projected from one, and a trait object or `impl Trait` name no
/// constructible type unless the signature supplies a `Default` bound.
///
/// `Self::Value` is deliberately excluded. Inside an `impl` block it resolves to the type chosen
/// by that block and may implement `Default`.
pub(super) fn is_abstract_type(ty: &Type, abstracts: &[String]) -> bool {
    match ty {
        Type::TraitObject(_) | Type::ImplTrait(_) => true,

        Type::Paren(paren) => is_abstract_type(&paren.elem, abstracts),

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
pub(super) fn undefaulted_parameters(generics: &Generics, defaults: &DefaultPaths) -> Vec<String> {
    let defaulted = standard_defaulted_parameters(generics, defaults);

    generics
        .params
        .iter()
        .filter_map(|param| match param {
            GenericParam::Type(ty) => (!defaulted.iter().any(|name| name == &ty.ident.to_string())).then(|| ty.ident.to_string()),

            _ => None,
        })
        .collect()
}
