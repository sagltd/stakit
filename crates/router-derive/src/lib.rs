//! The `#[action]` attribute macro for `stakit-router`.
//!
//! Turns a free function into a named action. The signature is flexible — async
//! or sync, and any of `(cx, params)`, `(params)`, `(cx)`, or `()`:
//!
//! ```ignore
//! #[action] async fn get_user(cx: &Cx<App, Auth>, params: GetUser) -> Result<User, AppError> { … }
//! #[action] fn ping() -> Result<&'static str, Error> { Ok("pong") }
//! #[action(stream)] fn ticks(cx: &Cx<App, Auth>) -> impl Stream<Item = Result<u64, Error>> { … }
//! ```
//!
//! A param-less action gets `Input = ()`. An action without `cx` is implemented
//! generically over any `G`/`R`. The error type is taken from the return, so
//! actions can return their own error type (anything `Into<Error>`).

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    AssocType, FnArg, GenericArgument, ItemFn, PatType, PathArguments, ReturnType, Type,
    parse_macro_input, spanned::Spanned,
};

/// See the crate docs.
#[proc_macro_attribute]
pub fn action(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    let is_stream = matches!(attr.to_string().trim(), "stream");
    expand(&func, is_stream)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(func: &ItemFn, is_stream: bool) -> syn::Result<proc_macro2::TokenStream> {
    let vis = &func.vis;
    let name = &func.sig.ident;
    let name_str = name.to_string();
    let block = &func.block;

    // Classify args: `&Cx<G, R>` is the context, the other is params. We keep the
    // user's written `Cx<…>` type so their `use Cx` / `use Error` stay "used".
    let mut cx: Option<(syn::Pat, Type, Type, Type)> = None;
    let mut params: Option<(syn::Pat, Type)> = None;
    for arg in &func.sig.inputs {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(
                arg.span(),
                "`#[action]` does not take `self`",
            ));
        };
        if let Some((cx_ty, g, r)) = cx_info(ty) {
            cx = Some(((**pat).clone(), cx_ty, g, r));
        } else {
            params = Some(((**pat).clone(), (**ty).clone()));
        }
    }

    let cx_bind = cx
        .as_ref()
        .map_or_else(|| quote!(_cx), |(p, ..)| quote!(#p));
    let (input_ty, params_bind) = match &params {
        Some((p, ty)) => (quote!(#ty), quote!(#p)),
        None => (quote!(()), quote!(_input)),
    };

    let (impl_header, g_ty, r_ty, cx_param_ty) = match &cx {
        Some((_, cx_ty, g, r)) => (quote!(impl), quote!(#g), quote!(#r), quote!(&'a #cx_ty)),
        None => (
            quote!(impl<G: ::core::marker::Send + ::core::marker::Sync + 'static, R: ::core::marker::Send + ::core::marker::Sync + 'static>),
            quote!(G),
            quote!(R),
            quote!(&'a ::stakit_router::Cx<G, R>),
        ),
    };

    let default_err = || syn::parse_quote!(::stakit_router::Error);

    if is_stream {
        let (item_ty, err_ty) = stream_item_types(&func.sig.output)?;
        let err_ty = err_ty.unwrap_or_else(default_err);
        Ok(quote! {
            #[allow(non_camel_case_types)]
            #vis struct #name;

            #impl_header ::stakit_router::StreamAction<#g_ty, #r_ty> for #name {
                type Input = #input_ty;
                type Item = #item_ty;
                type Error = #err_ty;

                fn name(&self) -> &'static str { #name_str }

                fn run<'a>(
                    &'a self,
                    #cx_bind: #cx_param_ty,
                    #params_bind: Self::Input,
                ) -> ::stakit_router::BoxStream<'a, ::core::result::Result<Self::Item, Self::Error>>
                {
                    ::std::boxed::Box::pin(#block)
                }
            }

            impl ::stakit_router::Endpoint for #name {
                type Params = #input_ty;
                type Output = #item_ty;
                const ACTION: &'static str = #name_str;
                const KIND: ::stakit_router::Kind = ::stakit_router::Kind::Stream;
            }
        })
    } else {
        let (output_ty, err_ty) =
            result_parts(return_type(&func.sig.output)?).ok_or_else(|| {
                syn::Error::new(
                    func.sig.output.span(),
                    "`#[action]` return type must be `Result<Output, _>`",
                )
            })?;
        let err_ty = err_ty.unwrap_or_else(default_err);
        Ok(quote! {
            #[allow(non_camel_case_types)]
            #vis struct #name;

            #impl_header ::stakit_router::Action<#g_ty, #r_ty> for #name {
                type Input = #input_ty;
                type Output = #output_ty;
                type Error = #err_ty;

                fn name(&self) -> &'static str { #name_str }

                fn run<'a>(
                    &'a self,
                    #cx_bind: #cx_param_ty,
                    #params_bind: Self::Input,
                ) -> ::stakit_router::BoxFuture<'a, ::core::result::Result<Self::Output, Self::Error>>
                {
                    ::std::boxed::Box::pin(async move #block)
                }
            }

            impl ::stakit_router::Endpoint for #name {
                type Params = #input_ty;
                type Output = #output_ty;
                const ACTION: &'static str = #name_str;
                const KIND: ::stakit_router::Kind = ::stakit_router::Kind::Unary;
            }
        })
    }
}

/// If `ty` is `&Cx<G, R>`, returns `(Cx<G,R>, G, R)`.
fn cx_info(ty: &Type) -> Option<(Type, Type, Type)> {
    let Type::Reference(reference) = ty else {
        return None;
    };
    let inner = (*reference.elem).clone();
    let Type::Path(path) = &inner else {
        return None;
    };
    let seg = path.path.segments.last()?;
    if seg.ident != "Cx" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    let mut types = args.args.iter().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    Some((inner.clone(), types.next()?, types.next()?))
}

fn return_type(output: &ReturnType) -> syn::Result<&Type> {
    match output {
        ReturnType::Type(_, ty) => Ok(ty),
        ReturnType::Default => Err(syn::Error::new(
            output.span(),
            "`#[action]` requires an explicit return type",
        )),
    }
}

/// `(O, Some(E))` from `Result<O, E>`, `(O, None)` from `Result<O>`.
fn result_parts(ty: &Type) -> Option<(Type, Option<Type>)> {
    let Type::Path(path) = ty else { return None };
    let seg = path.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    let mut types = args.args.iter().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    });
    Some((types.next()?, types.next()))
}

/// `(Item, Some(E))` from `impl Stream<Item = Result<Item, E>>`.
fn stream_item_types(output: &ReturnType) -> syn::Result<(Type, Option<Type>)> {
    let ty = return_type(output)?;
    let err = || {
        syn::Error::new(
            ty.span(),
            "`#[action(stream)]` must return `impl Stream<Item = Result<Item, _>>`",
        )
    };
    let Type::ImplTrait(it) = ty else {
        return Err(err());
    };
    for bound in &it.bounds {
        let syn::TypeParamBound::Trait(tb) = bound else {
            continue;
        };
        let Some(seg) = tb.path.segments.last() else {
            continue;
        };
        if seg.ident != "Stream" {
            continue;
        }
        let PathArguments::AngleBracketed(args) = &seg.arguments else {
            continue;
        };
        for arg in &args.args {
            if let GenericArgument::AssocType(AssocType {
                ident,
                ty: result_ty,
                ..
            }) = arg
            {
                if ident == "Item" {
                    return result_parts(result_ty).ok_or_else(err);
                }
            }
        }
    }
    Err(err())
}
