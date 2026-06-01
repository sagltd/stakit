//! Derive macro for [`stakit-model`](https://docs.rs/stakit-model).
//!
//! Provides `#[derive(Model)]`, which generates, from `#[validate(...)]`
//! field attributes:
//! 1. `impl stakit_model::Validate` — direct, inlined validation.
//! 2. `impl TSType` — a TypeScript `interface` (structs) or union (enums).
//!
//! See the `stakit-model` crate docs and `docs/architecture.md` for the
//! supported rule set and the generated TypeScript shapes.

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

/// One annotation for a model.
///
/// Derives `Model` + serde `Serialize`/`Deserialize`, and (under the `camel`
/// feature) injects `#[serde(rename_all = "camelCase")]` so the wire format
/// always matches the camelCase TypeScript — no per-struct serde attribute to
/// forget.
///
/// ```ignore
/// #[model]
/// struct CreateUser { #[validate(min_len = 3)] user_name: String }
/// ```
#[proc_macro_attribute]
pub fn model(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let rename = if cfg!(feature = "camel") {
        quote!(#[serde(rename_all = "camelCase")])
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
    let validate = emit_validate::expand(&ident, &ir);
    let ts = emit_ts::expand(&ident, &ir);
    Ok(quote::quote! {
        #validate
        #ts
    })
}
