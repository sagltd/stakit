//! Generates `impl TSType` (TypeScript export) from the parsed [`Ir`].
//!
//! Emits `ts_ref` (the type's reference) + `ts_declarations` (its `export …`
//! block, plus a recursive walk that registers every nested type). Generic types
//! are monomorphized: the reference for `Message<User>` is a concrete name
//! (`"MessageUser"`) and its field `data` references `User`, whose own
//! declaration is registered by the recursion.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{GenericArgument, Generics, Ident, PathArguments, Type};

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

/// `<Ty as TSType>::ts_ref()` for the type that should appear in output — the
/// inner type for `Option<T>`, else the field type itself.
fn field_ref_expr(ty: &Type) -> TokenStream {
    let render = option_inner(ty).unwrap_or(ty);
    quote!(<#render as ::stakit_model::TSType>::ts_ref())
}

/// All field types in declaration order (for the declarations recursion).
fn field_types(ir: &Ir) -> Vec<Type> {
    let mut out = Vec::new();
    let mut push_body = |body: &Body| match body {
        Body::Named(fields) | Body::Tuple(fields) => {
            out.extend(fields.iter().map(|f| f.ty.clone()));
        }
        Body::Unit => {}
    };
    match ir {
        Ir::Struct { body } => push_body(body),
        Ir::Enum { variants } => {
            for v in variants {
                push_body(&v.body);
            }
        }
    }
    out
}

pub(crate) fn expand(ident: &Ident, ir: &Ir, generics: &Generics) -> TokenStream {
    let name = ident.to_string();
    let type_params: Vec<&Ident> = generics.type_params().map(|p| &p.ident).collect();

    // impl header with `TSType` added to every type param.
    let mut bounded = generics.clone();
    for param in bounded.type_params_mut() {
        param.bounds.push(syn::parse_quote!(::stakit_model::TSType));
    }
    let (impl_generics, _, where_clause) = bounded.split_for_impl();
    let (_, ty_generics, _) = generics.split_for_impl();

    let ts_ref_fn = ts_ref_fn(&name, &type_params);
    let decl_expr = decl_expr(ir);
    let field_decls = field_types(ir).into_iter().map(|ty| {
        let render = option_inner(&ty).unwrap_or(&ty).clone();
        quote!(<#render as ::stakit_model::TSType>::ts_declarations(out);)
    });
    let param_decls = type_params
        .iter()
        .map(|p| quote!(<#p as ::stakit_model::TSType>::ts_declarations(out);));

    quote! {
        impl #impl_generics ::stakit_model::TSType for #ident #ty_generics #where_clause {
            #ts_ref_fn

            fn ts_declarations(out: &mut ::std::collections::BTreeMap<String, String>) {
                let __name = <Self as ::stakit_model::TSType>::ts_ref();
                if out.contains_key(&__name) {
                    return;
                }
                let __decl = #decl_expr;
                out.insert(__name, __decl);
                #(#field_decls)*
                #(#param_decls)*
            }
        }
    }
}

/// The `ts_ref` method: a plain name, or for generics the name with each type
/// argument's (alphanumeric-sanitized) reference appended → a unique concrete
/// name per instantiation.
fn ts_ref_fn(name: &str, type_params: &[&Ident]) -> TokenStream {
    if type_params.is_empty() {
        quote! {
            fn ts_ref() -> String {
                String::from(#name)
            }
        }
    } else {
        let appends = type_params.iter().map(|p| {
            quote! {
                __r.extend(
                    <#p as ::stakit_model::TSType>::ts_ref()
                        .chars()
                        .filter(|c| c.is_alphanumeric()),
                );
            }
        });
        quote! {
            fn ts_ref() -> String {
                let mut __r = String::from(#name);
                #(#appends)*
                __r
            }
        }
    }
}

/// Builds the `export …` declaration string for this type at runtime (its name
/// is `Self::ts_ref()`; field types use their `ts_ref`).
fn decl_expr(ir: &Ir) -> TokenStream {
    match ir {
        Ir::Struct { body } => struct_decl(body),
        Ir::Enum { variants } => enum_decl(variants),
    }
}

fn struct_decl(body: &Body) -> TokenStream {
    match body {
        Body::Named(fields) => {
            let lines = fields.iter().map(named_field_line);
            quote! {{
                let mut __ts = format!(
                    "export interface {} {{\n",
                    <Self as ::stakit_model::TSType>::ts_ref()
                );
                #(#lines)*
                __ts.push('}');
                __ts
            }}
        }
        Body::Tuple(fields) => {
            let exprs = fields.iter().map(|f| field_ref_expr(&f.ty));
            quote! {{
                let __items: &[String] = &[ #(#exprs),* ];
                format!(
                    "export type {} = [{}];",
                    <Self as ::stakit_model::TSType>::ts_ref(),
                    __items.join(", ")
                )
            }}
        }
        Body::Unit => quote! {
            format!("export type {} = null;", <Self as ::stakit_model::TSType>::ts_ref())
        },
    }
}

fn named_field_line(field: &Field) -> TokenStream {
    let label = crate::ir::wire_name(&field.label);
    let expr = field_ref_expr(&field.ty);
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

fn enum_decl(variants: &[Variant]) -> TokenStream {
    if variants.is_empty() {
        return quote! {
            format!("export type {} = never;", <Self as ::stakit_model::TSType>::ts_ref())
        };
    }
    let parts = variants.iter().map(variant_part);
    quote! {{
        let __parts: ::std::vec::Vec<String> = ::std::vec![ #(#parts),* ];
        format!(
            "export type {} = {};",
            <Self as ::stakit_model::TSType>::ts_ref(),
            __parts.join(" | ")
        )
    }}
}

fn variant_part(variant: &Variant) -> TokenStream {
    match &variant.body {
        Body::Unit => {
            let lit = format!("\"{}\"", variant.ident);
            quote! { String::from(#lit) }
        }
        Body::Named(fields) => {
            let entries = fields.iter().map(|f| {
                let expr = field_ref_expr(&f.ty);
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
                field_ref_expr(&fields[0].ty)
            } else {
                let exprs = fields.iter().map(|f| field_ref_expr(&f.ty));
                quote! {{
                    let __items: ::std::vec::Vec<String> = ::std::vec![ #(#exprs),* ];
                    format!("[{}]", __items.join(", "))
                }}
            }
        }
    }
}
