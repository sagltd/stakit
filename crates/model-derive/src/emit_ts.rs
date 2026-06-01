//! Generates `impl TSType` (TypeScript export) from the parsed [`Ir`].

use proc_macro2::TokenStream;
use quote::quote;
use syn::{GenericArgument, Ident, PathArguments, Type};

use crate::ir::{Body, Field, Ir, Variant};

/// Returns the inner type `T` if `ty` is `Option<T>`.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// `<Ty as TSType>::to_ts()` for the type that should appear in output —
/// the inner type for `Option<T>`, else the field type itself.
fn field_ts_expr(ty: &Type) -> TokenStream {
    let render = option_inner(ty).unwrap_or(ty);
    quote!(<#render as ::stakit_model::TSType>::to_ts())
}

pub(crate) fn expand(ident: &Ident, ir: &Ir) -> TokenStream {
    let name = ident.to_string();
    let body = match ir {
        Ir::Struct { body } => struct_ts(&name, body),
        Ir::Enum { variants } => enum_ts(&name, variants),
    };
    quote! {
        impl ::stakit_model::TSType for #ident {
            fn to_ts() -> String {
                #body
            }
        }
    }
}

fn struct_ts(name: &str, body: &Body) -> TokenStream {
    match body {
        Body::Named(fields) => {
            let header = format!("export interface {name} {{\n");
            let lines = fields.iter().map(named_field_line);
            quote! {
                let mut __ts = String::from(#header);
                #(#lines)*
                __ts.push('}');
                __ts
            }
        }
        Body::Tuple(fields) => {
            let exprs = fields.iter().map(|f| field_ts_expr(&f.ty));
            let prefix = format!("export type {name} = ");
            quote! {
                let __items: &[String] = &[ #(#exprs),* ];
                format!("{}[{}];", #prefix, __items.join(", "))
            }
        }
        Body::Unit => {
            let s = format!("export type {name} = null;");
            quote! { String::from(#s) }
        }
    }
}

fn named_field_line(field: &Field) -> TokenStream {
    let label = crate::ir::wire_name(&field.label);
    let expr = field_ts_expr(&field.ty);
    let sep = if option_inner(&field.ty).is_some() {
        format!("  {label}?: ")
    } else {
        format!("  {label}: ")
    };
    quote! {
        __ts.push_str(#sep);
        __ts.push_str(&#expr);
        __ts.push_str(";\n");
    }
}

fn enum_ts(name: &str, variants: &[Variant]) -> TokenStream {
    let parts = variants.iter().map(variant_part);
    let prefix = format!("export type {name} = ");
    quote! {
        let __parts: ::std::vec::Vec<String> = ::std::vec![ #(#parts),* ];
        format!("{}{};", #prefix, __parts.join(" | "))
    }
}

fn variant_part(variant: &Variant) -> TokenStream {
    match &variant.body {
        Body::Unit => {
            let lit = format!("\"{}\"", variant.ident);
            quote! { String::from(#lit) }
        }
        Body::Named(fields) => {
            let entries = fields.iter().map(|f| {
                let expr = field_ts_expr(&f.ty);
                let name = crate::ir::wire_name(&f.label);
                let head = if option_inner(&f.ty).is_some() {
                    format!("{name}?: ")
                } else {
                    format!("{name}: ")
                };
                quote! {{ let mut __e = String::from(#head); __e.push_str(&#expr); __e }}
            });
            quote! {{
                let __fields: ::std::vec::Vec<String> = ::std::vec![ #(#entries),* ];
                format!("{{ {} }}", __fields.join("; "))
            }}
        }
        Body::Tuple(fields) => {
            if fields.len() == 1 {
                field_ts_expr(&fields[0].ty)
            } else {
                let exprs = fields.iter().map(|f| field_ts_expr(&f.ty));
                quote! {{
                    let __items: ::std::vec::Vec<String> = ::std::vec![ #(#exprs),* ];
                    format!("[{}]", __items.join(", "))
                }}
            }
        }
    }
}
