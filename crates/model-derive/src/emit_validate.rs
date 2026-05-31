//! Generates `impl garde::Validate` from the parsed [`Ir`].

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

use crate::ir::{Body, Field, Ir, Rule, Variant};

/// Path to the re-exported `garde` crate inside generated code.
fn garde() -> TokenStream {
    quote!(::stakit_model::__private::garde)
}

pub(crate) fn expand(ident: &Ident, ir: &Ir) -> TokenStream {
    let g = garde();
    let body = match ir {
        Ir::Struct { body } => struct_body(body),
        Ir::Enum { variants } => enum_body(variants),
    };
    quote! {
        impl #g::Validate for #ident {
            type Context = ();

            #[allow(clippy::needless_borrow, unused_variables)]
            fn validate_into(
                &self,
                __garde_ctx: &Self::Context,
                mut __garde_path: &mut dyn FnMut() -> #g::Path,
                __garde_report: &mut #g::Report,
            ) {
                let __garde_user_ctx = &__garde_ctx;
                #body
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
        Body::Named(fields) => {
            let pat = named_pattern(fields);
            let blocks = fields.iter().filter(|f| validated(f)).map(field_block);
            quote! {
                let Self #pat = self;
                #(#blocks)*
            }
        }
        Body::Tuple(fields) => {
            let pat = tuple_pattern(fields);
            let blocks = fields.iter().filter(|f| validated(f)).map(field_block);
            quote! {
                let Self #pat = self;
                #(#blocks)*
            }
        }
    }
}

fn enum_body(variants: &[Variant]) -> TokenStream {
    let arms = variants.iter().map(|v| {
        let vident = &v.ident;
        match &v.body {
            Body::Unit => quote! { Self::#vident => {} },
            Body::Named(fields) => {
                let pat = named_pattern(fields);
                let blocks = fields.iter().filter(|f| validated(f)).map(field_block);
                quote! { Self::#vident #pat => { #(#blocks)* } }
            }
            Body::Tuple(fields) => {
                let pat = tuple_pattern(fields);
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

fn named_pattern(fields: &[Field]) -> TokenStream {
    let bindings = fields.iter().filter(|f| validated(f)).map(|f| &f.binding);
    quote! { { #(#bindings,)* .. } }
}

fn tuple_pattern(fields: &[Field]) -> TokenStream {
    // Bind every slot positionally; unused bindings are allowed on the fn.
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

fn field_block(field: &Field) -> TokenStream {
    let label = &field.label;
    let binding = &field.binding;
    let rules = field.rules.iter().map(rule_tokens);
    quote! {
        {
            let mut __garde_path = ::stakit_model::__private::nested_path!(__garde_path, #label);
            let __garde_binding = &*#binding;
            #(#rules)*
        }
    }
}

fn append_on_err(call: &TokenStream) -> TokenStream {
    quote! {
        if let Err(__garde_error) = #call {
            __garde_report.append(__garde_path(), __garde_error);
        }
    }
}

fn rule_tokens(rule: &Rule) -> TokenStream {
    let g = garde();
    match rule {
        Rule::Email => append_on_err(&quote!((#g::rules::email::apply)(&*__garde_binding, ()))),
        Rule::Url => append_on_err(&quote!((#g::rules::url::apply)(&*__garde_binding, ()))),
        Rule::Ascii => append_on_err(&quote!((#g::rules::ascii::apply)(&*__garde_binding, ()))),
        Rule::Alphanumeric => {
            append_on_err(&quote!((#g::rules::alphanumeric::apply)(&*__garde_binding, ())))
        }
        Rule::Length { min, max, equal } => {
            let args = equal.as_ref().map_or_else(
                || {
                    let lo = min
                        .as_ref()
                        .map_or_else(|| quote!(0usize), |m| quote!((#m) as usize));
                    let hi = max
                        .as_ref()
                        .map_or_else(|| quote!(usize::MAX), |m| quote!((#m) as usize));
                    quote!((#lo, #hi))
                },
                |eq| quote!(((#eq) as usize, (#eq) as usize)),
            );
            append_on_err(&quote!((#g::rules::length::simple::apply)(&*__garde_binding, #args)))
        }
        Rule::Range { min, max, equal } => {
            let args = equal.as_ref().map_or_else(
                || {
                    let lo = min
                        .as_ref()
                        .map_or_else(|| quote!(None), |m| quote!(Some(#m)));
                    let hi = max
                        .as_ref()
                        .map_or_else(|| quote!(None), |m| quote!(Some(#m)));
                    quote!((#lo, #hi))
                },
                |eq| quote!((Some(#eq), Some(#eq))),
            );
            append_on_err(&quote!((#g::rules::range::apply)(&*__garde_binding, #args)))
        }
        Rule::Contains(e) => {
            append_on_err(&quote!((#g::rules::contains::apply)(&*__garde_binding, (#e,))))
        }
        Rule::Prefix(e) => {
            append_on_err(&quote!((#g::rules::prefix::apply)(&*__garde_binding, (#e,))))
        }
        Rule::Suffix(e) => {
            append_on_err(&quote!((#g::rules::suffix::apply)(&*__garde_binding, (#e,))))
        }
        Rule::Pattern(lit) => {
            let call = append_on_err(
                &quote!((#g::rules::pattern::apply)(&*__garde_binding, (&__GARDE_PATTERN,))),
            );
            quote! {
                {
                    static __GARDE_PATTERN: #g::rules::pattern::regex::StaticPattern =
                        #g::rules::pattern::regex::init_pattern!(#lit);
                    #call
                }
            }
        }
        Rule::Custom(f) => quote! {
            if let Err(__garde_error) = (#f)(&*__garde_binding, &__garde_user_ctx) {
                __garde_report.append(__garde_path(), __garde_error);
            }
        },
        Rule::Dive => quote! {
            #g::validate::Validate::validate_into(
                &*__garde_binding,
                __garde_user_ctx,
                &mut __garde_path,
                __garde_report,
            );
        },
    }
}
