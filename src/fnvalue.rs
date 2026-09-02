// Copyright 2021-2024 Martin Pool

//! Mutations of replacing a function body with a value of a (hopefully) appropriate type.

#![warn(clippy::pedantic)]

use std::collections::HashMap;
use std::iter;

use itertools::Itertools;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    AngleBracketedGenericArguments, AssocType, Expr, GenericArgument, Ident, Path, PathArguments,
    ReturnType, TraitBound, Type, TypeArray, TypeImplTrait, TypeParamBound, TypeSlice, TypeTuple,
};
use tracing::trace;

use crate::pretty::ToPrettyString;

/// Generate replacement text for a function based on its return type.
pub(crate) fn return_type_replacements(
    return_type: &ReturnType,
    error_exprs: &[Expr],
    enums: &EnumIndex,
) -> Vec<TokenStream> {
    match return_type {
        ReturnType::Default => vec![quote! { () }],
        ReturnType::Type(_rarrow, type_) => {
            type_replacements(type_, error_exprs, enums).collect_vec()
        }
    }
}

/// Unit variants of the enums declared in the file being visited, by enum name.
///
/// Only same-file enums: resolving a type to a declaration in another file or
/// crate would need a workspace-wide type index, which this crate deliberately
/// does not have.
#[derive(Debug, Default)]
pub(crate) struct EnumIndex(HashMap<String, Vec<Ident>>);

impl EnumIndex {
    /// Collect every enum declared in the file, including inside inline `mod` blocks.
    pub(crate) fn from_file(file: &syn::File) -> EnumIndex {
        let mut index = EnumIndex::default();
        index.add_items(&file.items);
        index
    }

    fn add_items(&mut self, items: &[syn::Item]) {
        for item in items {
            match item {
                syn::Item::Enum(item_enum) => {
                    let unit_variants = item_enum
                        .variants
                        .iter()
                        .filter(|v| matches!(v.fields, syn::Fields::Unit))
                        .map(|v| v.ident.clone())
                        .collect_vec();
                    if !unit_variants.is_empty() {
                        self.0.insert(item_enum.ident.to_string(), unit_variants);
                    }
                }
                syn::Item::Mod(item_mod) => {
                    if let Some((_brace, items)) = &item_mod.content {
                        self.add_items(items);
                    }
                }
                _ => {}
            }
        }
    }

    /// Unit variants of the enum this path names, if it names a same-file enum
    /// that has any.
    fn unit_variants<'s, 'p>(&'s self, path: &'p Path) -> Option<(&'p Ident, &'s [Ident])> {
        let last = path.segments.last()?;
        let variants = self.0.get(&last.ident.to_string())?;
        Some((&last.ident, variants))
    }
}

/// Generate some values that we hope are reasonable replacements for a type.
#[allow(clippy::too_many_lines)]
fn type_replacements(
    type_: &Type,
    error_exprs: &[Expr],
    enums: &EnumIndex,
) -> impl Iterator<Item = TokenStream> {
    // This could probably change to run from some configuration rather than
    // hardcoding various types, which would make it easier to support tree-specific
    // mutation values, and perhaps reduce duplication. However, it seems better
    // to support all the core cases with direct code first to learn what generalizations
    // are needed.
    match type_ {
        Type::Path(syn::TypePath { path, .. }) => {
            // dbg!(&path);
            if path.is_ident("bool") {
                vec![quote! { true }, quote! { false }]
            } else if path.is_ident("String") {
                vec![quote! { String::new() }, quote! { "xyzzy".into() }]
            } else if path.is_ident("str") {
                vec![quote! { "" }, quote! { "xyzzy" }]
            } else if path.is_ident("char") {
                vec![quote! { '\0' }, quote! { 'x' }]
            } else if let Some(ident) = path_ident_in(path, &["PathBuf", "Utf8PathBuf"]) {
                // Use the matched ident, not a fixed `PathBuf`: camino's
                // `Utf8PathBuf` mirrors the std API but is a different type.
                vec![quote! { #ident::new() }, quote! { #ident::from("xyzzy") }]
            } else if path_ends_with(path, "OsString") {
                vec![
                    quote! { OsString::new() },
                    quote! { OsString::from("xyzzy") },
                ]
            } else if path_ends_with(path, "Duration") {
                vec![quote! { Duration::ZERO }, quote! { Duration::from_secs(1) }]
            } else if path_is_unsigned(path) {
                vec![quote! { 0 }, quote! { 1 }]
            } else if path_is_signed(path) {
                vec![quote! { 0 }, quote! { 1 }, quote! { -1 }]
            } else if path_is_nonzero_signed(path) {
                vec![
                    quote! { 1.try_into().unwrap() },
                    quote! { (-1).try_into().unwrap() },
                ]
            } else if path_is_nonzero_unsigned(path) {
                vec![quote! { 1.try_into().unwrap() }]
            } else if let Some(inner_type) = match_first_type_arg(path, "NonZero") {
                // NonZero<T> generic form (stabilized in Rust 1.79)
                if let Type::Path(syn::TypePath {
                    path: inner_path, ..
                }) = inner_type
                    && path_is_unsigned(inner_path)
                {
                    // NonZero<T> where T is an unsigned type can only be positive.
                    vec![quote! { 1.try_into().unwrap() }]
                } else {
                    // NonZero<T> where T is a signed type or an unknown type could be positive or negative.
                    vec![
                        quote! { 1.try_into().unwrap() },
                        quote! { (-1).try_into().unwrap() },
                    ]
                }
            } else if path_is_float(path) {
                vec![quote! { 0.0 }, quote! { 1.0 }, quote! { -1.0 }]
            } else if path_is_cmp_ordering(path) {
                vec![
                    quote! { Ordering::Less },
                    quote! { Ordering::Equal },
                    quote! { Ordering::Greater },
                ]
            } else if path_ends_with(path, "Result") {
                let ok_reps = if let Some(ok_type) = match_first_type_arg(path, "Result") {
                    type_replacements(ok_type, error_exprs, enums)
                        .map(|rep| quote! { Ok(#rep) })
                        .collect_vec()
                } else {
                    // A result with no type arguments, like `fmt::Result`; hopefully
                    // the Ok value can be constructed with Default.
                    vec![quote! { Ok(Default::default()) }]
                };
                let configured_errs = error_exprs
                    .iter()
                    .map(|error_expr| quote! { Err(#error_expr) })
                    .collect_vec();
                // Only recurse into the error type when we know how to build a value
                // for it without relying on `Default`, which errors rarely implement.
                let recursed_errs = match_nth_type_arg(path, "Result", 1)
                    .filter(|err_type| is_simply_constructible(err_type))
                    .map(|err_type| {
                        type_replacements(err_type, error_exprs, enums)
                            .map(|rep| quote! { Err(#rep) })
                            .collect_vec()
                    })
                    .unwrap_or_default();
                ok_reps
                    .into_iter()
                    .chain(configured_errs)
                    .chain(recursed_errs)
                    .unique_by(ToPrettyString::to_pretty_string)
                    .collect_vec()
            } else if path_ends_with(path, "HttpResponse") {
                vec![quote! { HttpResponse::Ok().finish() }]
            } else if let Some(some_type) = match_first_type_arg(path, "Option") {
                iter::once(quote! { None })
                    .chain(type_replacements(some_type, error_exprs, enums).map(|rep| {
                        quote! { Some(#rep) }
                    }))
                    .collect_vec()
            } else if let Some(element_type) = match_first_type_arg(path, "Vec") {
                // Generate an empty Vec, and then a one-element vec for every recursive
                // value.
                iter::once(quote! { vec![] })
                    .chain(
                        type_replacements(element_type, error_exprs, enums).map(|rep| {
                            quote! { vec![#rep] }
                        }),
                    )
                    .collect_vec()
            } else if let Some(borrowed_type) = match_first_type_arg(path, "Cow") {
                // TODO: We could specialize Cows for cases like Vec and Box where
                // we would have to leak to make the reference; perhaps it would only
                // look better...
                type_replacements(borrowed_type, error_exprs, enums)
                    .flat_map(|rep| {
                        [
                            quote! { Cow::Borrowed(#rep) },
                            quote! { Cow::Owned(#rep.to_owned()) },
                        ]
                    })
                    .collect_vec()
            } else if path_ident_in(path, &["Pin", "NonNull"]).is_some() {
                // Every candidate generated for these fails to compile
                // (E0061/E0599/E0308/E0277), so generating them only burns a build.
                vec![]
            } else if path_ident_in(path, &["Instant", "SystemTime"]).is_some() {
                // No public constructor that takes a value, and no `Default`.
                vec![]
            } else if path_ident_in(path, &["Range"]).is_some() {
                // `Range<T>` has a `Default` impl, but has one type argument and so
                // would otherwise be caught by `maybe_collection_or_container` below
                // and turned into `Range::new()` / `Range::from_iter(..)`, none of
                // which compile.
                vec![quote! { Default::default() }]
            } else if let Some((container_type, inner_type)) = known_container(path) {
                // Something like Arc, Mutex, etc.
                // TODO: Ideally we should use the path without relying on it being
                // imported, but we must strip or rewrite the arguments, so that
                // `std::sync::Arc<String>` becomes either `std::sync::Arc::<String>::new`
                // or at least `std::sync::Arc::new`. Similarly for other types.
                type_replacements(inner_type, error_exprs, enums)
                    .map(|rep| {
                        quote! { #container_type::new(#rep) }
                    })
                    .collect_vec()
            } else if let Some((collection_type, inner_type)) = known_collection(path) {
                iter::once(quote! { #collection_type::new() })
                    .chain(
                        type_replacements(inner_type, error_exprs, enums).map(|rep| {
                            quote! { #collection_type::from_iter([#rep]) }
                        }),
                    )
                    .collect_vec()
            } else if let Some((collection_type, key_type, value_type)) = known_map(path) {
                let key_reps = type_replacements(key_type, error_exprs, enums).collect_vec();
                let val_reps = type_replacements(value_type, error_exprs, enums).collect_vec();
                iter::once(quote! { #collection_type::new() })
                    .chain(
                        key_reps
                            .iter()
                            .cartesian_product(val_reps)
                            .map(|(k, v)| quote! { #collection_type::from_iter([(#k, #v)]) }),
                    )
                    .collect_vec()
            } else if let Some((collection_type, inner_type)) = maybe_collection_or_container(path)
            {
                // Something like `T<A>` or `T<'a, A>`, when we don't know exactly how
                // to call it, but we strongly suspect that you could construct it from
                // an `A`.
                iter::once(quote! { #collection_type::new() })
                    .chain(
                        type_replacements(inner_type, error_exprs, enums).flat_map(|rep| {
                            [
                                quote! { #collection_type::from_iter([#rep]) },
                                quote! { #collection_type::new(#rep) },
                                quote! { #collection_type::from(#rep) },
                            ]
                        }),
                    )
                    .collect_vec()
            } else if let Some((enum_ident, variants)) = enums.unit_variants(path) {
                // A same-file enum: its unit variants are values we know exist.
                // `Default::default()` is not also emitted: for an enum deriving
                // `Default` it would duplicate one of these, and otherwise it
                // would be unviable.
                variants
                    .iter()
                    .map(|variant| quote! { #enum_ident::#variant })
                    .collect_vec()
            } else {
                trace!(
                    type_ = type_.to_pretty_string(),
                    "Return type is not recognized, trying Default"
                );
                vec![quote! { Default::default() }]
            }
        }
        Type::Array(TypeArray { elem, len, .. }) =>
        // Generate arrays that repeat each replacement value however many times.
        // In principle we could generate combinations, but that might get very
        // large, and values like "all zeros" and "all ones" seem likely to catch
        // lots of things.
        {
            type_replacements(elem, error_exprs, enums)
                .map(|r| quote! { [ #r; #len ] })
                .collect_vec()
        }
        Type::Slice(TypeSlice { elem, .. }) => iter::once(quote! { Vec::leak(Vec::new()) })
            .chain(
                type_replacements(elem, error_exprs, enums)
                    .map(|r| quote! { Vec::leak(vec![ #r ]) }),
            )
            .collect_vec(),
        Type::Reference(syn::TypeReference {
            mutability: None,
            elem,
            ..
        }) => match &**elem {
            // Mutate non-mutable references to static strings, and references to slices to a
            // leaked vec, and otherwise to simple references to values.
            // TODO: Also mutate references to single values?
            // You can't currently match box patterns in Rust
            Type::Path(path) if path.path.is_ident("str") => {
                vec![quote! { "" }, quote! { "xyzzy" }]
            }
            Type::Slice(TypeSlice { elem, .. }) => iter::once(quote! { Vec::leak(Vec::new()) })
                .chain(
                    type_replacements(elem, error_exprs, enums)
                        .map(|r| quote! { Vec::leak(vec![ #r ]) }),
                )
                .collect_vec(),
            _ => type_replacements(elem, error_exprs, enums)
                .map(|rep| {
                    quote! { Box::leak(Box::new(#rep)) }
                })
                .collect_vec(),
        },
        Type::Reference(syn::TypeReference {
            mutability: Some(_),
            elem,
            ..
        }) => match &**elem {
            Type::Slice(TypeSlice { elem, .. }) => iter::once(quote! { Vec::leak(Vec::new()) })
                .chain(
                    type_replacements(elem, error_exprs, enums)
                        .map(|r| quote! { Vec::leak(vec![ #r ]) }),
                )
                .collect_vec(),
            _ => {
                // Make &mut with static lifetime by leaking them on the heap.
                type_replacements(elem, error_exprs, enums)
                    .map(|rep| {
                        quote! { Box::leak(Box::new(#rep)) }
                    })
                    .collect_vec()
            }
        },
        Type::Tuple(TypeTuple { elems, .. }) => {
            // Generate the cartesian product of replacements of every type within the tuple.
            elems
                .iter()
                .map(|elem| type_replacements(elem, error_exprs, enums).collect_vec())
                .multi_cartesian_product()
                .map(|reps| {
                    quote! { ( #( #reps ),* ) }
                })
                .collect_vec()
        }
        // -> impl Iterator<Item = T>
        Type::ImplTrait(impl_trait) => {
            if let Some(item_type) = match_impl_iterator(impl_trait) {
                iter::once(quote! { ::std::iter::empty() })
                    .chain(
                        type_replacements(item_type, error_exprs, enums)
                            .map(|r| quote! { ::std::iter::once(#r) }),
                    )
                    .collect_vec()
            } else {
                // TODO: Can we do anything with other impl traits?
                vec![]
            }
        }
        Type::TraitObject(_) => {
            // `dyn Trait` can't be constructed by value; via `Box<dyn Trait>` this
            // makes the whole function generate no FnValue mutants, which is correct:
            // `Box::new(Default::default())` never compiles (E0790).
            vec![]
        }
        Type::Never(_) => {
            vec![]
        }
        _ => {
            trace!(?type_, "Return type is not recognized, trying Default");
            vec![quote! { Default::default() }]
        }
    }
    .into_iter()
}

fn path_ends_with(path: &Path, ident: &str) -> bool {
    path.segments.last().is_some_and(|s| s.ident == ident)
}

fn match_impl_iterator(TypeImplTrait { bounds, .. }: &TypeImplTrait) -> Option<&Type> {
    for bound in bounds {
        if let TypeParamBound::Trait(TraitBound { path, .. }) = bound
            && let Some(last_segment) = path.segments.last()
            && last_segment.ident == "Iterator"
            && let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
                &last_segment.arguments
            && let Some(GenericArgument::AssocType(AssocType { ident, ty, .. })) = args.first()
            && ident == "Item"
        {
            return Some(ty);
        }
    }
    None
}

/// If the type has a single type argument then, perhaps it's a simple container
/// like Box, Cell, Mutex, etc, that can be constructed with `T::new(inner_val)`.
///
/// If so, return the short name (like "Box") and the inner type.
fn known_container(path: &Path) -> Option<(&Ident, &Type)> {
    let last = path.segments.last()?;
    if ["Box", "Cell", "RefCell", "Arc", "Rc", "Mutex"]
        .iter()
        .any(|v| last.ident == v)
        && let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
            &last.arguments
    {
        // TODO: Skip lifetime args.
        // TODO: Return the path with args stripped out.
        if args.len() == 1
            && let Some(GenericArgument::Type(inner_type)) = args.first()
        {
            return Some((&last.ident, inner_type));
        }
    }
    None
}

/// Match known simple collections that can be empty or constructed from an
/// iterator.
///
/// Returns the short name (like `"VecDeque"`) and the inner type.
fn known_collection(path: &Path) -> Option<(&Ident, &Type)> {
    let last = path.segments.last()?;
    if ![
        "BinaryHeap",
        "BTreeSet",
        "HashSet",
        "LinkedList",
        "VecDeque",
    ]
    .iter()
    .any(|v| last.ident == v)
    {
        return None;
    }
    if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    {
        // TODO: Skip lifetime args.
        // TODO: Return the path with args stripped out.
        if args.len() == 1
            && let Some(GenericArgument::Type(inner_type)) = args.first()
        {
            return Some((&last.ident, inner_type));
        }
    }
    None
}

/// Match known key-value maps that can be empty or constructed from pair of
/// recursively-generated values.
fn known_map(path: &Path) -> Option<(&Ident, &Type, &Type)> {
    let last = path.segments.last()?;
    if !["BTreeMap", "HashMap"].iter().any(|v| last.ident == v) {
        return None;
    }
    if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    {
        // TODO: Skip lifetime args.
        // TODO: Return the path with args stripped out.
        if let Some((GenericArgument::Type(key_type), GenericArgument::Type(value_type))) =
            args.iter().collect_tuple()
        {
            return Some((&last.ident, key_type, value_type));
        }
    }
    None
}
/// Match a type with one type argument, which might be a container or collection.
fn maybe_collection_or_container(path: &Path) -> Option<(&Ident, &Type)> {
    let last = path.segments.last()?;
    if let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    {
        let type_args: Vec<_> = args
            .iter()
            .filter_map(|a| match a {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect();
        // TODO: Return the path with args stripped out.
        if type_args.len() == 1 {
            return Some((&last.ident, type_args.first().unwrap()));
        }
    }
    None
}

fn path_is_float(path: &Path) -> bool {
    ["f32", "f64"].iter().any(|s| path.is_ident(s))
}

fn path_is_unsigned(path: &Path) -> bool {
    ["u8", "u16", "u32", "u64", "u128", "usize"]
        .iter()
        .any(|s| path.is_ident(s))
}

fn path_is_signed(path: &Path) -> bool {
    ["i8", "i16", "i32", "i64", "i128", "isize"]
        .iter()
        .any(|s| path.is_ident(s))
}

fn path_is_nonzero_signed(path: &Path) -> bool {
    if let Some(l) = path.segments.last().map(|p| p.ident.to_string()) {
        matches!(
            l.as_str(),
            "NonZeroIsize"
                | "NonZeroI8"
                | "NonZeroI16"
                | "NonZeroI32"
                | "NonZeroI64"
                | "NonZeroI128",
        )
    } else {
        false
    }
}

fn path_is_nonzero_unsigned(path: &Path) -> bool {
    if let Some(l) = path.segments.last().map(|p| p.ident.to_string()) {
        matches!(
            l.as_str(),
            "NonZeroUsize"
                | "NonZeroU8"
                | "NonZeroU16"
                | "NonZeroU32"
                | "NonZeroU64"
                | "NonZeroU128",
        )
    } else {
        false
    }
}

/// If this is a path ending in `expected_ident`, return the first type argument, ignoring
/// lifetimes.
fn match_first_type_arg<'p>(path: &'p Path, expected_ident: &str) -> Option<&'p Type> {
    // TODO: Maybe match only things with one arg?
    let last = path.segments.last()?;
    if last.ident == expected_ident
        && let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
            &last.arguments
    {
        for arg in args {
            match arg {
                GenericArgument::Type(arg_type) => return Some(arg_type),
                GenericArgument::Lifetime(_) => (),
                _ => return None,
            }
        }
    }
    None
}

/// True for `Ordering` and `cmp::Ordering`, but not `atomic::Ordering`.
fn path_is_cmp_ordering(path: &Path) -> bool {
    path_ends_with(path, "Ordering") && !path.segments.iter().any(|s| s.ident == "atomic")
}

/// If the last segment of the path is one of `names`, return its ident.
fn path_ident_in<'p>(path: &'p Path, names: &[&str]) -> Option<&'p Ident> {
    let last = path.segments.last()?;
    names.iter().any(|n| last.ident == n).then_some(&last.ident)
}

/// If this is a path ending in `expected_ident`, return its `index`th type argument,
/// ignoring lifetime arguments.
fn match_nth_type_arg<'p>(path: &'p Path, expected_ident: &str, index: usize) -> Option<&'p Type> {
    let last = path.segments.last()?;
    if last.ident != expected_ident {
        return None;
    }
    let PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) =
        &last.arguments
    else {
        return None;
    };
    args.iter()
        .filter_map(|a| match a {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .nth(index)
}

/// True for types whose replacement values are literals we know compile without
/// relying on `Default`, used to decide whether recursing is worthwhile.
fn is_simply_constructible(type_: &Type) -> bool {
    match type_ {
        Type::Path(syn::TypePath { path, .. }) => {
            path.is_ident("bool")
                || path.is_ident("String")
                || path.is_ident("str")
                || path.is_ident("char")
                || path_is_unsigned(path)
                || path_is_signed(path)
                || path_is_float(path)
        }
        Type::Reference(syn::TypeReference { elem, .. }) => matches!(
            &**elem,
            Type::Path(syn::TypePath { path, .. }) if path.is_ident("str")
        ),
        _ => false,
    }
}

#[cfg(test)]
mod test {
    use itertools::Itertools;
    use pretty_assertions::assert_eq;
    use syn::{Expr, ReturnType, parse_quote};

    use crate::fnvalue::match_impl_iterator;
    use crate::pretty::ToPrettyString;

    use super::{EnumIndex, known_map, return_type_replacements};

    #[test]
    fn recurse_into_result_bool() {
        check_replacements(
            &parse_quote! {-> std::result::Result<bool> },
            &[],
            &["Ok(true)", "Ok(false)"],
        );
    }

    #[test]
    fn recurse_into_result_result_bool_with_error_values() {
        check_replacements(
            &parse_quote! {-> std::result::Result<Result<bool>> },
            &[parse_quote! { anyhow!("mutated") }],
            &[
                "Ok(Ok(true))",
                "Ok(Ok(false))",
                r#"Ok(Err(anyhow!("mutated")))"#,
                r#"Err(anyhow!("mutated"))"#,
            ],
        );
    }

    #[test]
    fn u16_replacements() {
        check_replacements(&parse_quote! { -> u16 }, &[], &["0", "1"]);
    }

    #[test]
    fn isize_replacements() {
        check_replacements(&parse_quote! { -> isize }, &[], &["0", "1", "-1"]);
    }

    #[test]
    fn nonzero_integer_replacements() {
        check_replacements(
            &parse_quote! { -> std::num::NonZeroIsize },
            &[],
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        );

        check_replacements(
            &parse_quote! { -> std::num::NonZeroUsize },
            &[],
            &["1.try_into().unwrap()"],
        );

        check_replacements(
            &parse_quote! { -> std::num::NonZeroU32 },
            &[],
            &["1.try_into().unwrap()"],
        );
    }

    #[test]
    fn nonzero_generic_unsigned_replacements() {
        check_replacements(
            &parse_quote! { -> NonZero<u32> },
            &[],
            &["1.try_into().unwrap()"],
        );

        check_replacements(
            &parse_quote! { -> NonZero<usize> },
            &[],
            &["1.try_into().unwrap()"],
        );

        check_replacements(
            &parse_quote! { -> std::num::NonZero<u8> },
            &[],
            &["1.try_into().unwrap()"],
        );
    }

    #[test]
    fn nonzero_generic_signed_replacements() {
        check_replacements(
            &parse_quote! { -> NonZero<i32> },
            &[],
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        );

        check_replacements(
            &parse_quote! { -> NonZero<isize> },
            &[],
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        );

        check_replacements(
            &parse_quote! { -> std::num::NonZero<i64> },
            &[],
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        );
    }

    #[test]
    fn nonzero_generic_unknown_type_replacements() {
        // When T is not a recognized integer type, assume it could be signed.
        check_replacements(
            &parse_quote! { -> NonZero<T> },
            &[],
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        );
    }

    #[test]
    fn unit_replacement() {
        check_replacements(&parse_quote! { -> () }, &[], &["()"]);
    }

    #[test]
    fn result_unit_replacement() {
        check_replacements(&parse_quote! { -> Result<(), Error> }, &[], &["Ok(())"]);

        check_replacements(&parse_quote! { -> Result<()> }, &[], &["Ok(())"]);
    }

    #[test]
    fn http_response_replacement() {
        check_replacements(
            &parse_quote! { -> HttpResponse },
            &[],
            &["HttpResponse::Ok().finish()"],
        );
    }

    #[test]
    fn option_usize_replacement() {
        check_replacements(
            &parse_quote! { -> Option<usize> },
            &[],
            &["None", "Some(0)", "Some(1)"],
        );
    }

    #[test]
    fn box_usize_replacement() {
        check_replacements(
            &parse_quote! { -> Box<usize> },
            &[],
            &["Box::new(0)", "Box::new(1)"],
        );
    }

    #[test]
    fn box_unrecognized_type_replacement() {
        check_replacements(
            &parse_quote! { -> Box<MyObject> },
            &[],
            &["Box::new(Default::default())"],
        );
    }

    #[test]
    fn vec_string_replacement() {
        check_replacements(
            &parse_quote! { -> std::vec::Vec<String> },
            &[],
            &["vec![]", "vec![String::new()]", r#"vec!["xyzzy".into()]"#],
        );
    }

    #[test]
    fn float_replacement() {
        check_replacements(&parse_quote! { -> f32 }, &[], &["0.0", "1.0", "-1.0"]);
    }

    #[test]
    fn ref_replacement_leaks_values() {
        // To avoid returning references to temporary values, the mutation of references
        // leaks values onto the heap.
        check_replacements(
            &parse_quote! { -> &'static String },
            &[],
            &[
                "Box::leak(Box::new(String::new()))",
                "Box::leak(Box::new(\"xyzzy\".into()))",
            ],
        );
    }

    #[test]
    fn ref_replacement_recurses() {
        check_replacements(
            &parse_quote! { -> &bool },
            &[],
            &["Box::leak(Box::new(true))", "Box::leak(Box::new(false))"],
        );
    }

    #[test]
    fn ref_mut() {
        check_replacements(
            &parse_quote! { -> &mut bool },
            &[],
            &["Box::leak(Box::new(true))", "Box::leak(Box::new(false))"],
        );
    }

    #[test]
    fn array_replacement() {
        check_replacements(
            &parse_quote! { -> [u8; 256] },
            &[],
            &["[0; 256]", "[1; 256]"],
        );
    }

    #[test]
    fn arc_replacement() {
        // Also checks that it matches the path, even using an atypical path.
        // TODO: Ideally this would be fully qualified like `alloc::sync::Arc::new(String::new())`.
        check_replacements(
            &parse_quote! { -> alloc::sync::Arc<String> },
            &[],
            &["Arc::new(String::new())", r#"Arc::new("xyzzy".into())"#],
        );
    }

    #[test]
    fn rc_replacement() {
        // Also checks that it matches the path, even using an atypical path.
        // TODO: Ideally this would be fully qualified like `alloc::sync::Rc::new(String::new())`.
        check_replacements(
            &parse_quote! { -> alloc::sync::Rc<String> },
            &[],
            &["Rc::new(String::new())", r#"Rc::new("xyzzy".into())"#],
        );
    }

    #[test]
    fn match_known_collection() {
        assert_eq!(
            super::known_collection(&parse_quote! { std::collections::VecDeque<String> }),
            Some((&parse_quote! { VecDeque }, &parse_quote! { String }))
        );

        assert_eq!(
            super::known_collection(&parse_quote! { std::collections::BinaryHeap<(u32, u32)> }),
            Some((&parse_quote! { BinaryHeap }, &parse_quote! { (u32, u32) }))
        );

        assert_eq!(
            super::known_collection(&parse_quote! { LinkedList<[u8; 256]> }),
            Some((&parse_quote! { LinkedList }, &parse_quote! { [u8; 256] }))
        );

        assert_eq!(super::known_collection(&parse_quote! { Arc<String> }), None);

        // This might be a collection, and is handled generically, but it's not a specifically known
        // collection type. (Maybe we shouldn't bother knowing specific types?)
        assert_eq!(
            super::known_collection(&parse_quote! { Wibble<&str> }),
            None
        );
    }

    #[test]
    fn match_known_map() {
        assert_eq!(
            super::known_map(&parse_quote! { std::collections::BTreeMap<String, usize> }),
            Some((
                &parse_quote! { BTreeMap },
                &parse_quote! { String },
                &parse_quote! { usize }
            ))
        );

        assert_eq!(
            super::known_map(&parse_quote! { std::collections::HashMap<(usize, usize), bool> }),
            Some((
                &parse_quote! { HashMap },
                &parse_quote! { (usize, usize) },
                &parse_quote! { bool }
            ))
        );

        assert_eq!(
            super::known_map(&parse_quote! { Option<(usize, usize)> }),
            None
        );

        assert_eq!(
            super::known_map(&parse_quote! { MyMap<String, usize> }),
            None,
        );

        assert_eq!(
            super::known_map(&parse_quote! { Pair<String, usize> }),
            None,
        );
    }

    #[test]
    fn btreeset_replacement() {
        check_replacements(
            &parse_quote! { -> std::collections::BTreeSet<String> },
            &[],
            &[
                "BTreeSet::new()",
                "BTreeSet::from_iter([String::new()])",
                r#"BTreeSet::from_iter(["xyzzy".into()])"#,
            ],
        );
    }

    #[test]
    fn cow_generates_borrowed_and_owned() {
        check_replacements(
            &parse_quote! { -> Cow<'static, str> },
            &[],
            &[
                r#"Cow::Borrowed("")"#,
                r#"Cow::Owned("".to_owned())"#,
                r#"Cow::Borrowed("xyzzy")"#,
                r#"Cow::Owned("xyzzy".to_owned())"#,
            ],
        );
    }

    #[test]
    fn unknown_container_replacement() {
        // This looks like something that holds a &str, and maybe can be constructed
        // from a &str, but we don't know anything else about it, so we just guess.
        check_replacements(
            &parse_quote! { -> UnknownContainer<'static, str> },
            &[],
            &[
                "UnknownContainer::new()",
                r#"UnknownContainer::from_iter([""])"#,
                r#"UnknownContainer::new("")"#,
                r#"UnknownContainer::from("")"#,
                r#"UnknownContainer::from_iter(["xyzzy"])"#,
                r#"UnknownContainer::new("xyzzy")"#,
                r#"UnknownContainer::from("xyzzy")"#,
            ],
        );
    }

    #[test]
    fn tuple_combinations() {
        check_replacements(
            &parse_quote! { -> (bool, usize) },
            &[],
            &["(true, 0)", "(true, 1)", "(false, 0)", "(false, 1)"],
        );
    }

    #[test]
    fn tuple_combination_longer() {
        check_replacements(
            &parse_quote! { -> (bool, Option<String>) },
            &[],
            &[
                "(true, None)",
                "(true, Some(String::new()))",
                r#"(true, Some("xyzzy".into()))"#,
                "(false, None)",
                "(false, Some(String::new()))",
                r#"(false, Some("xyzzy".into()))"#,
            ],
        );
    }

    #[test]
    fn iter_replacement() {
        check_replacements(
            &parse_quote! { -> impl Iterator<Item = String> },
            &[],
            &[
                "::std::iter::empty()",
                "::std::iter::once(String::new())",
                r#"::std::iter::once("xyzzy".into())"#,
            ],
        );
    }

    #[test]
    fn impl_matches_iterator() {
        assert_eq!(
            match_impl_iterator(&parse_quote! { impl std::iter::Iterator<Item = String> }),
            Some(&parse_quote! { String })
        );
        assert_eq!(
            match_impl_iterator(&parse_quote! { impl Iterator<Item = String> }),
            Some(&parse_quote! { String })
        );
        // Strange, maybe it's a type defined in this crate, but we don't know what to
        // do with it.
        assert_eq!(match_impl_iterator(&parse_quote! { impl Iterator }), None);
        assert_eq!(
            match_impl_iterator(&parse_quote! { impl Borrow<String> }),
            None
        );
    }

    #[test]
    fn slice_replacement() {
        check_replacements(
            &parse_quote! { -> [u8] },
            &[],
            &[
                "Vec::leak(Vec::new())",
                "Vec::leak(vec![0])",
                "Vec::leak(vec![1])",
            ],
        );
    }

    #[test]
    fn btreemap_replacement() {
        check_replacements(
            &parse_quote! { -> BTreeMap<String, bool> },
            &[],
            &[
                "BTreeMap::new()",
                "BTreeMap::from_iter([(String::new(), true)])",
                "BTreeMap::from_iter([(String::new(), false)])",
                "BTreeMap::from_iter([(\"xyzzy\".into(), true)])",
                "BTreeMap::from_iter([(\"xyzzy\".into(), false)])",
            ],
        );
    }

    #[test]
    fn cmp_ordering_replacements() {
        check_replacements(
            &parse_quote! { -> std::cmp::Ordering },
            &[],
            &["Ordering::Less", "Ordering::Equal", "Ordering::Greater"],
        );
    }

    #[test]
    fn range_replacement_is_default_not_a_guessed_constructor() {
        check_replacements(
            &parse_quote! { -> Range<i32> },
            &[],
            &["Default::default()"],
        );
    }

    #[test]
    fn unconstructible_types_generate_no_replacements() {
        for return_type in [
            parse_quote! { -> Box<dyn std::error::Error> },
            parse_quote! { -> Pin<Box<String>> },
            parse_quote! { -> NonNull<u8> },
            parse_quote! { -> Instant },
            parse_quote! { -> SystemTime },
        ] {
            check_replacements(&return_type, &[], &[]);
        }
    }

    #[test]
    fn recurse_into_result_error_type_when_simply_constructible() {
        check_replacements(
            &parse_quote! { -> Result<bool, String> },
            &[],
            &[
                "Ok(true)",
                "Ok(false)",
                "Err(String::new())",
                "Err(\"xyzzy\".into())",
            ],
        );
    }

    #[test]
    fn dont_recurse_into_opaque_result_error_type() {
        check_replacements(
            &parse_quote! { -> Result<bool, anyhow::Error> },
            &[],
            &["Ok(true)", "Ok(false)"],
        );
    }

    #[test]
    fn char_replacements() {
        check_replacements(&parse_quote! { -> char }, &[], &["'\\0'", "'x'"]);
    }

    #[test]
    fn path_buf_replacements() {
        check_replacements(
            &parse_quote! { -> std::path::PathBuf },
            &[],
            &["PathBuf::new()", "PathBuf::from(\"xyzzy\")"],
        );
    }

    #[test]
    fn utf8_path_buf_replacements_use_the_matched_type() {
        check_replacements(
            &parse_quote! { -> camino::Utf8PathBuf },
            &[],
            &["Utf8PathBuf::new()", "Utf8PathBuf::from(\"xyzzy\")"],
        );
    }

    #[test]
    fn os_string_replacements() {
        check_replacements(
            &parse_quote! { -> std::ffi::OsString },
            &[],
            &["OsString::new()", "OsString::from(\"xyzzy\")"],
        );
    }

    #[test]
    fn duration_replacements() {
        check_replacements(
            &parse_quote! { -> std::time::Duration },
            &[],
            &["Duration::ZERO", "Duration::from_secs(1)"],
        );
    }

    fn check_replacements(return_type: &ReturnType, error_exprs: &[Expr], expected: &[&str]) {
        check_replacements_with_enums(return_type, error_exprs, &EnumIndex::default(), expected);
    }

    fn check_replacements_with_enums(
        return_type: &ReturnType,
        error_exprs: &[Expr],
        enums: &EnumIndex,
        expected: &[&str],
    ) {
        assert_eq!(
            return_type_replacements(return_type, error_exprs, enums)
                .into_iter()
                .map(|t| t.to_pretty_string())
                .collect_vec(),
            expected
        );
    }

    #[test]
    fn same_file_enum_unit_variants_are_replacements() {
        let file: syn::File = parse_quote! {
            enum Colour {
                Red,
                Green,
                Custom(u32),
            }
        };
        check_replacements_with_enums(
            &parse_quote! { -> Colour },
            &[],
            &EnumIndex::from_file(&file),
            &["Colour::Red", "Colour::Green"],
        );
    }

    #[test]
    fn enum_in_inline_mod_is_indexed() {
        let file: syn::File = parse_quote! {
            mod inner {
                enum Flag {
                    On,
                    Off,
                }
            }
        };
        check_replacements_with_enums(
            &parse_quote! { -> inner::Flag },
            &[],
            &EnumIndex::from_file(&file),
            &["Flag::On", "Flag::Off"],
        );
    }

    #[test]
    fn enum_without_unit_variants_falls_back_to_default() {
        let file: syn::File = parse_quote! {
            enum Wrapper {
                A(u32),
            }
        };
        check_replacements_with_enums(
            &parse_quote! { -> Wrapper },
            &[],
            &EnumIndex::from_file(&file),
            &["Default::default()"],
        );
    }

    #[test]
    fn match_map() {
        assert!(known_map(&parse_quote! { BTreeMap<String, usize> }).is_some());
        assert!(known_map(&parse_quote! { HashMap<(usize, usize), bool> }).is_some());
        assert!(known_map(&parse_quote! { Option<(usize, usize)> }).is_none());
    }
}
