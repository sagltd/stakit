//! Shared `syn` helpers for stakit's proc-macro crates (`#[action]`, `#[tool]`,
//! `#[derive(Model)]`). Not a public API: it exists so the derive crates don't
//! each carry their own copy of the same type-shape matching.

use syn::{FnArg, GenericArgument, ItemFn, Pat, PatType, PathArguments, Type, spanned::Spanned};

/// The type arguments of `ty` when it is a path whose last segment is
/// `name<…>`: `Option<T>` with `"Option"` yields `T`, `Result<O, E>` with
/// `"Result"` yields `O`, `E`. `None` for any other shape.
pub fn type_args<'a>(ty: &'a Type, name: &str) -> Option<impl Iterator<Item = &'a Type>> {
    let Type::Path(path) = ty else { return None };
    let seg = path.path.segments.last()?;
    if seg.ident != name {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    Some(args.args.iter().filter_map(|arg| match arg {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    }))
}

/// A free function's context parameter (`pattern`, whatever `cx` extracted from
/// its type) — if any.
pub type CxParam<'a, C> = Option<(&'a Pat, C)>;

/// A free function's other (input) parameter, `(pattern, type)` — if any.
pub type InputParam<'a> = Option<(&'a Pat, &'a Type)>;

/// Splits a macro-annotated free function's parameters into its context
/// parameter (the one `cx` recognises) and its input parameter (any other).
/// A later parameter of either kind replaces an earlier one.
///
/// # Errors
///
/// A `self` receiver: `attr` (e.g. `` `#[tool]` ``) names the macro in the
/// message.
pub fn split_params<'a, C>(
    func: &'a ItemFn,
    attr: &str,
    cx: impl Fn(&'a Type) -> Option<C>,
) -> syn::Result<(CxParam<'a, C>, InputParam<'a>)> {
    let mut cx_param = None;
    let mut input = None;
    for arg in &func.sig.inputs {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(
                arg.span(),
                format!("{attr} does not take `self`"),
            ));
        };
        if let Some(extracted) = cx(ty) {
            cx_param = Some((&**pat, extracted));
        } else {
            input = Some((&**pat, &**ty));
        }
    }
    Ok((cx_param, input))
}
