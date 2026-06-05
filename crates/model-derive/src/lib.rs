//! Derive macro for [`stakit-model`](https://docs.rs/stakit-model).
//!
//! Provides `#[derive(Model)]`, which generates, from `#[validate(...)]`
//! field attributes:
//! 1. `impl stakit_model::Validate` — direct, inlined validation.
//! 2. `impl TSType` — a TypeScript `interface` (structs) or union (enums).
//!
//! See the `stakit-model` crate docs and `docs/architecture.md` for the
//! supported rule set and the generated TypeScript shapes.

#[cfg(feature = "schema")]
mod emit_jsonschema;
mod emit_ts;
mod emit_validate;
mod ir;

use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse_macro_input};

/// Derives `Model` (validation + TypeScript export) for a struct or enum.
#[proc_macro_derive(Model, attributes(validate))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives `JsonSchema` (a JSON Schema fragment) for a struct or enum.
///
/// Reuses the `#[validate(...)]` rule grammar to lower constraints to schema
/// keywords (`min_len`→`minLength`, `min`→`minimum`, `pattern`→`pattern`, …)
/// and `///` doc-comments / `#[arg(description = "…")]` to property
/// descriptions. Opt-in (the `schema` feature); pair with `#[derive(Model)]`.
#[cfg(feature = "schema")]
#[proc_macro_derive(JsonSchema, attributes(validate, arg))]
pub fn derive_json_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    ir::parse(&input)
        .map_or_else(syn::Error::into_compile_error, |(ident, ir)| {
            emit_jsonschema::expand(&ident, &ir)
        })
        .into()
}

/// One annotation for a model.
///
/// Derives `Model` + serde `Serialize`/`Deserialize`, and (under the `camel`
/// feature) injects the serde rename that makes the wire format match the
/// camelCase TypeScript + validation paths — no per-type serde attribute to
/// forget. A struct gets `#[serde(rename_all = "camelCase")]` (renames its
/// fields); an enum gets `#[serde(rename_all_fields = "camelCase")]` (renames
/// struct-variant payload fields, leaving the externally-tagged variant names
/// verbatim, which is how the TypeScript export renders them).
///
/// Prefer `#[model]` over a bare `#[derive(Model)]` when the `camel` feature is
/// on: the derive camelCases the generated TypeScript + validation paths
/// unconditionally, so a bare derive *without* the matching serde rename would
/// produce camelCase TypeScript over a `snake_case` wire. `#[model]` keeps the
/// two in lockstep.
///
/// ```ignore
/// #[model]
/// struct CreateUser { #[validate(min_len = 3)] user_name: String }
/// ```
#[proc_macro_attribute]
pub fn model(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    // The serde rename must match how the generated TypeScript + validation paths
    // are camelCased (`ir::wire_name`). For a struct, `rename_all` camelCases its
    // fields. For an enum, `rename_all` would camelCase the *variant tags* (which
    // the TS export keeps verbatim) — the payload field names are renamed by
    // `rename_all_fields`, so an enum gets that one instead, leaving variant tags
    // PascalCase on both the wire and in the TypeScript.
    let rename = if cfg!(feature = "camel") {
        if matches!(input.data, syn::Data::Enum(_)) {
            quote!(#[serde(rename_all_fields = "camelCase")])
        } else {
            quote!(#[serde(rename_all = "camelCase")])
        }
    } else {
        quote!()
    };
    quote! {
        #[derive(::stakit_model::Model, ::serde::Serialize, ::serde::Deserialize)]
        #rename
        #input
    }
    .into()
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let (ident, ir) = ir::parse(input)?;
    let validate = emit_validate::expand(&ident, &ir, &input.generics);
    let ts = emit_ts::expand(&ident, &ir, &input.generics);
    Ok(quote::quote! {
        #validate
        #ts
    })
}
