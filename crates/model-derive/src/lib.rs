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
use syn::{DeriveInput, parse_macro_input};

/// Derives `Model` (validation + TypeScript export) for a struct or enum.
#[proc_macro_derive(Model, attributes(validate))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
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
