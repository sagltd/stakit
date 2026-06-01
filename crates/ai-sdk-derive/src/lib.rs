//! The `#[tool]` attribute macro for `stakit-ai-sdk`.
//!
//! Turns a free function into a [`Tool`]. The signature is flexible — async or
//! sync, and any of `(cx, args)`, `(args)`, `(cx)`, or `()`:
//!
//! ```ignore
//! #[tool] async fn weather(cx: &ToolCx<App>, args: WeatherArgs) -> Result<Report, ToolError> { … }
//! #[tool] async fn now(args: TzArgs) -> Result<String, ToolError> { … }
//! #[tool(description = "ping the server")] fn ping() -> Result<&'static str, ToolError> { Ok("pong") }
//! ```
//!
//! The tool name defaults to the function name (override with
//! `#[tool(name = "…")]`); the description defaults to the function's
//! doc-comment (override with `#[tool(description = "…")]`). A tool without a
//! `cx` parameter is implemented generically over any context type. The
//! arguments type must derive `Model` + `JsonSchema`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, ItemFn, Lit, Meta, PatType, PathArguments, ReturnType,
    Token, Type, parse_macro_input, punctuated::Punctuated, spanned::Spanned,
};

/// See the crate docs.
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let args = match parse_attr(attr) {
        Ok(a) => a,
        Err(e) => return e.into_compile_error().into(),
    };
    expand(&func, &args)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(Default)]
struct ToolArgs {
    name: Option<String>,
    description: Option<String>,
}

fn parse_attr(attr: TokenStream) -> syn::Result<ToolArgs> {
    let mut out = ToolArgs::default();
    if attr.is_empty() {
        return Ok(out);
    }
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr)?;
    for meta in metas {
        let Meta::NameValue(nv) = meta else {
            return Err(syn::Error::new(
                meta.span(),
                "`#[tool]` expects `name = \"…\"` / `description = \"…\"`",
            ));
        };
        let value = lit_string(&nv.value)?;
        if nv.path.is_ident("name") {
            out.name = Some(value);
        } else if nv.path.is_ident("description") {
            out.description = Some(value);
        } else {
            return Err(syn::Error::new(nv.path.span(), "unknown `#[tool]` key"));
        }
    }
    Ok(out)
}

fn lit_string(expr: &Expr) -> syn::Result<String> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(s), ..
    }) = expr
    {
        Ok(s.value())
    } else {
        Err(syn::Error::new(expr.span(), "expected a string literal"))
    }
}

use syn::parse::Parser as _;

fn expand(func: &ItemFn, args: &ToolArgs) -> syn::Result<proc_macro2::TokenStream> {
    let vis = &func.vis;
    let fn_name = &func.sig.ident;
    let block = &func.block;

    let tool_name = args.name.clone().unwrap_or_else(|| fn_name.to_string());
    let description = args
        .description
        .clone()
        .or_else(|| doc_comment(func))
        .unwrap_or_default();

    // Classify args: `&ToolCx<Ctx>` is the context; the other is the args type.
    let mut cx: Option<(syn::Pat, Type)> = None;
    let mut params: Option<(syn::Pat, Type)> = None;
    for arg in &func.sig.inputs {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(
                arg.span(),
                "`#[tool]` does not take `self`",
            ));
        };
        if let Some(ctx_ty) = cx_ctx_type(ty) {
            cx = Some(((**pat).clone(), ctx_ty));
        } else {
            params = Some(((**pat).clone(), (**ty).clone()));
        }
    }

    let cx_bind = cx.as_ref().map_or_else(|| quote!(_cx), |(p, _)| quote!(#p));
    let (args_ty, args_bind) = match &params {
        Some((p, ty)) => (quote!(#ty), quote!(#p)),
        None => (quote!(()), quote!(_args)),
    };

    let (impl_header, ctx_ty) = match &cx {
        Some((_, ctx)) => (quote!(impl), quote!(#ctx)),
        None => (
            quote!(impl<Ctx: ::core::marker::Send + ::core::marker::Sync + 'static>),
            quote!(Ctx),
        ),
    };

    let output_ty = result_output(&func.sig.output).ok_or_else(|| {
        syn::Error::new(
            func.sig.output.span(),
            "`#[tool]` return type must be `Result<Output, ToolError>`",
        )
    })?;

    Ok(quote! {
        #[allow(non_camel_case_types)]
        #vis struct #fn_name;

        #impl_header ::stakit_ai_sdk::Tool<#ctx_ty> for #fn_name {
            type Args = #args_ty;
            type Output = #output_ty;

            fn name(&self) -> &'static str { #tool_name }
            fn description(&self) -> &'static str { #description }

            fn run<'a>(
                &'a self,
                #cx_bind: &'a ::stakit_ai_sdk::ToolCx<#ctx_ty>,
                #args_bind: Self::Args,
            ) -> ::stakit_ai_sdk::BoxFuture<'a, ::core::result::Result<Self::Output, ::stakit_ai_sdk::ToolError>>
            {
                ::std::boxed::Box::pin(async move #block)
            }
        }
    })
}

/// If `ty` is `&ToolCx<Ctx>`, returns the `Ctx` type.
fn cx_ctx_type(ty: &Type) -> Option<Type> {
    let Type::Reference(reference) = ty else {
        return None;
    };
    let Type::Path(path) = &*reference.elem else {
        return None;
    };
    let seg = path.path.segments.last()?;
    if seg.ident != "ToolCx" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

/// Extracts `Output` from a `Result<Output, _>` return type.
fn result_output(output: &ReturnType) -> Option<Type> {
    let ReturnType::Type(_, ty) = output else {
        return None;
    };
    let Type::Path(path) = &**ty else { return None };
    let seg = path.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let PathArguments::AngleBracketed(generics) = &seg.arguments else {
        return None;
    };
    generics.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

/// Joins the function's `///` doc-comments into a description.
fn doc_comment(func: &ItemFn) -> Option<String> {
    let mut lines = Vec::new();
    for attr in &func.attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Ok(s) = lit_string(&nv.value) {
                lines.push(s.trim().to_owned());
            }
        }
    }
    let joined = lines.join("\n").trim().to_owned();
    (!joined.is_empty()).then_some(joined)
}
