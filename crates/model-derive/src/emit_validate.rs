//! Generates `impl stakit_model::Validate` from the parsed [`Ir`].
//!
//! Strategy: destructure `self`, then for each rule emit a direct call to the
//! matching `stakit_model::validate::*` function, pushing any error (tagged
//! with the field path) into a stack-inline `ValidationErrors`. The happy path
//! allocates nothing.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Expr, Ident};

use crate::ir::{Body, Field, Ir, Rule, Variant};

/// Path to the `validate` module in generated code.
fn v() -> TokenStream {
    quote!(::stakit_model::validate)
}

pub(crate) fn expand(ident: &Ident, ir: &Ir) -> TokenStream {
    let body = match ir {
        Ir::Struct { body } => struct_body(body),
        Ir::Enum { variants } => enum_body(variants),
    };
    quote! {
        impl ::stakit_model::Validate for #ident {
            fn validate(&self) -> ::core::result::Result<(), ::stakit_model::ValidationErrors> {
                let mut __errors = ::stakit_model::ValidationErrors::new();
                #body
                ::stakit_model::ValidationErrors::into_result(__errors)
            }
        }
    }
}

fn validated(field: &Field) -> bool {
    !field.skip && !field.rules.is_empty()
}

fn struct_body(body: &Body) -> TokenStream {
    match body {
        Body::Unit => quote! {},
        Body::Named(fields) | Body::Tuple(fields) => {
            let pat = pattern(body, fields);
            let blocks = fields.iter().filter(|f| validated(f)).map(field_block);
            quote! {
                let Self #pat = self;
                #(#blocks)*
            }
        }
    }
}

fn enum_body(variants: &[Variant]) -> TokenStream {
    let arms = variants.iter().map(|var| {
        let vident = &var.ident;
        match &var.body {
            Body::Unit => quote! { Self::#vident => {} },
            Body::Named(fields) | Body::Tuple(fields) => {
                let pat = pattern(&var.body, fields);
                let blocks = fields.iter().filter(|f| validated(f)).map(field_block);
                quote! { Self::#vident #pat => { #(#blocks)* } }
            }
        }
    });
    quote! {
        match self {
            #(#arms)*
        }
    }
}

fn pattern(body: &Body, fields: &[Field]) -> TokenStream {
    match body {
        Body::Named(_) => {
            let bindings = fields.iter().filter(|f| validated(f)).map(|f| &f.binding);
            quote! { { #(#bindings,)* .. } }
        }
        Body::Tuple(_) => {
            let bindings = fields.iter().map(|f| {
                if validated(f) {
                    let b = &f.binding;
                    quote!(#b)
                } else {
                    quote!(_)
                }
            });
            quote! { ( #(#bindings),* ) }
        }
        Body::Unit => quote! {},
    }
}

fn field_block(field: &Field) -> TokenStream {
    let label = &field.label;
    let binding = &field.binding;
    let rules = field
        .rules
        .iter()
        .map(|rule| rule_tokens(rule, binding, label));
    quote! { #(#rules)* }
}

/// `Some(expr)` / `None` for an optional bound.
fn opt(expr: Option<&Expr>) -> TokenStream {
    expr.map_or_else(
        || quote!(::core::option::Option::None),
        |e| quote!(::core::option::Option::Some(#e)),
    )
}

fn rule_tokens(rule: &Rule, binding: &Ident, label: &str) -> TokenStream {
    let v = v();
    // Push a single-error rule result, tagged with the field path.
    let push = |call: TokenStream| {
        quote! {
            if let ::core::result::Result::Err(__e) = #call {
                __errors.push(__e.at_field(#label));
            }
        }
    };

    match rule {
        Rule::Email => push(quote!(#v::email(#binding))),
        Rule::Url => push(quote!(#v::url(#binding))),
        Rule::Ascii => push(quote!(#v::ascii(#binding))),
        Rule::Alphanumeric => push(quote!(#v::alphanumeric(#binding))),
        Rule::Contains(e) => push(quote!(#v::contains(#binding, #e))),
        Rule::Prefix(e) => push(quote!(#v::prefix(#binding, #e))),
        Rule::Suffix(e) => push(quote!(#v::suffix(#binding, #e))),
        Rule::Length { min, max } => {
            let (min, max) = (opt(min.as_ref()), opt(max.as_ref()));
            push(quote!(#v::length(#binding, #min, #max)))
        }
        Rule::Range { min, max } => {
            let (min, max) = (opt(min.as_ref()), opt(max.as_ref()));
            push(quote!(#v::range(#binding, #min, #max)))
        }
        Rule::Custom(f) => push(quote!((#f)(#binding))),
        Rule::Pattern(lit) => {
            let inner = push(quote!(#v::pattern(#binding, &__STAKIT_PATTERN)));
            quote! {
                {
                    static __STAKIT_PATTERN: ::std::sync::LazyLock<#v::Regex> =
                        ::std::sync::LazyLock::new(|| {
                            #v::Regex::new(#lit).expect("invalid regex in #[validate(pattern = ...)]")
                        });
                    #inner
                }
            }
        }
        Rule::Dive => quote! {
            if let ::core::result::Result::Err(__errs) =
                ::stakit_model::Validate::validate(#binding)
            {
                __errors.extend(__errs.into_iter().map(|__e| __e.at_field(#label)));
            }
        },
    }
}
