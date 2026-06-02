//! `#[derive(ResponseError)]` — generates `stakit_router::ResponseError` +
//! `stakit_router::ErrorCodes` impls from `#[status(...)]` / `#[code(...)]` /
//! `#[message(...)]` attributes.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitInt, LitStr, spanned::Spanned};

/// Parsed `#[status]` / `#[code]` / `#[message]` for one variant (or the whole
/// struct).
struct Attrs {
    status: LitInt,
    code: Option<LitStr>,
    message: Option<LitStr>,
}

/// Reads the three helper attributes off an attribute list. `#[status(...)]` is
/// required; the others are optional.
fn parse_attrs(attrs: &[syn::Attribute], span: proc_macro2::Span) -> syn::Result<Attrs> {
    let mut status = None;
    let mut code = None;
    let mut message = None;
    for attr in attrs {
        if attr.path().is_ident("status") {
            status = Some(attr.parse_args::<LitInt>()?);
        } else if attr.path().is_ident("code") {
            code = Some(attr.parse_args::<LitStr>()?);
        } else if attr.path().is_ident("message") {
            message = Some(attr.parse_args::<LitStr>()?);
        }
    }
    let status = status.ok_or_else(|| {
        syn::Error::new(
            span,
            "missing `#[status(...)]` (the HTTP status for this error)",
        )
    })?;
    Ok(Attrs {
        status,
        code,
        message,
    })
}

/// Converts a `CamelCase` identifier to a `snake_case` code (the default
/// machine code when `#[code(...)]` is absent).
fn snake_case(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 4);
    let chars: Vec<char> = ident.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_uppercase() && i != 0 {
            let prev_lower = chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            if prev_lower || (chars[i - 1].is_uppercase() && next_lower) {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// The machine code string for one set of attributes: the explicit `#[code]`,
/// else the `snake_case` of the identifier.
fn code_string(attrs: &Attrs, ident: &str) -> String {
    attrs
        .code
        .as_ref()
        .map_or_else(|| snake_case(ident), syn::LitStr::value)
}

/// `message()` body expression for one set of attributes.
fn message_expr(attrs: &Attrs) -> TokenStream {
    attrs.message.as_ref().map_or_else(
        || {
            quote!(::std::borrow::Cow::Owned(
                ::std::string::ToString::to_string(self)
            ))
        },
        |lit| quote!(::std::borrow::Cow::Borrowed(#lit)),
    )
}

/// A `Self::Variant`/`Self::Variant(..)`/`Self::Variant { .. }` match pattern.
fn variant_pattern(ident: &syn::Ident, fields: &Fields) -> TokenStream {
    match fields {
        Fields::Unit => quote!(Self::#ident),
        Fields::Unnamed(_) => quote!(Self::#ident(..)),
        Fields::Named(_) => quote!(Self::#ident { .. }),
    }
}

/// Expands the derive.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    // `codes`: every machine code this type can produce (for TS generation).
    let mut codes: Vec<String> = Vec::new();

    let (status_body, code_body, message_body) = match &input.data {
        Data::Struct(_) => {
            let attrs = parse_attrs(&input.attrs, input.span())?;
            let status = &attrs.status;
            let code = code_string(&attrs, &ident.to_string());
            let message = message_expr(&attrs);
            codes.push(code.clone());
            (
                quote!(#status),
                quote!(::std::borrow::Cow::Borrowed(#code)),
                message,
            )
        }
        Data::Enum(data) => {
            let mut status_arms = Vec::new();
            let mut code_arms = Vec::new();
            let mut message_arms = Vec::new();
            for variant in &data.variants {
                let attrs = parse_attrs(&variant.attrs, variant.span())?;
                let pat = variant_pattern(&variant.ident, &variant.fields);
                let status = &attrs.status;
                let code = code_string(&attrs, &variant.ident.to_string());
                let message = message_expr(&attrs);
                status_arms.push(quote!(#pat => #status,));
                code_arms.push(quote!(#pat => ::std::borrow::Cow::Borrowed(#code),));
                message_arms.push(quote!(#pat => #message,));
                codes.push(code);
            }
            (
                quote!(match self { #(#status_arms)* }),
                quote!(match self { #(#code_arms)* }),
                quote!(match self { #(#message_arms)* }),
            )
        }
        Data::Union(_) => {
            return Err(syn::Error::new(
                input.span(),
                "`ResponseError` cannot be derived for unions",
            ));
        }
    };

    Ok(quote! {
        impl #impl_generics ::stakit_router::ResponseError for #ident #ty_generics #where_clause {
            fn status(&self) -> u16 {
                #status_body
            }
            fn code(&self) -> ::std::borrow::Cow<'static, str> {
                #code_body
            }
            fn message(&self) -> ::std::borrow::Cow<'_, str> {
                #message_body
            }
        }

        impl #impl_generics ::stakit_router::ErrorCodes for #ident #ty_generics #where_clause {
            fn error_codes() -> &'static [&'static str] {
                &[ #(#codes),* ]
            }
        }
    })
}
