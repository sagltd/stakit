//! Generates `impl JsonSchema` (JSON Schema draft 2020-12 fragment) from the
//! parsed [`Ir`].
//!
//! Strategy mirrors [`emit_ts`](crate::emit_ts): walk the IR and build a
//! `serde_json::Value` at runtime (constraints carry runtime `syn::Expr`
//! bounds, and nested schemas come from `<T as JsonSchema>::schema()`).
//! `#[validate(...)]` rules lower to schema keywords; rules without a schema
//! analogue (`ascii`, `alphanumeric`, `contains`, `prefix`, `suffix`, `custom`)
//! are still enforced at runtime by `Validate`, just not described here.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{GenericArgument, Ident, PathArguments, Type};

use crate::ir::{Body, Field, Ir, Rule, Variant};

/// Path to the re-exported `serde_json` in generated code.
fn sj() -> TokenStream {
    quote!(::stakit_model::__serde_json)
}

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

pub(crate) fn expand(ident: &Ident, ir: &Ir) -> TokenStream {
    let sj = sj();
    let body = match ir {
        Ir::Struct { body } => struct_schema(body),
        Ir::Enum { variants } => enum_schema(variants),
    };
    quote! {
        impl ::stakit_model::JsonSchema for #ident {
            fn schema() -> #sj::Value {
                #body
            }
        }
    }
}

fn struct_schema(body: &Body) -> TokenStream {
    match body {
        Body::Named(fields) => object_schema(fields),
        Body::Tuple(fields) => tuple_schema(fields),
        Body::Unit => {
            let sj = sj();
            quote! { #sj::json!({ "type": "null" }) }
        }
    }
}

/// `{ "type": "object", "properties": {…}, "required": [...] }`.
fn object_schema(fields: &[Field]) -> TokenStream {
    let sj = sj();
    let props = fields.iter().map(named_property);
    let required = fields.iter().filter(|f| !is_optional(f)).map(|f| {
        let label = crate::ir::wire_name(&f.label);
        quote! { __required.push(#sj::Value::String(::std::string::String::from(#label))); }
    });
    quote! {{
        let mut __props = #sj::Map::new();
        let mut __required: ::std::vec::Vec<#sj::Value> = ::std::vec::Vec::new();
        #(#props)*
        #(#required)*
        let mut __root = #sj::Map::new();
        __root.insert(::std::string::String::from("type"), #sj::Value::String(::std::string::String::from("object")));
        __root.insert(::std::string::String::from("properties"), #sj::Value::Object(__props));
        if !__required.is_empty() {
            __root.insert(::std::string::String::from("required"), #sj::Value::Array(__required));
        }
        #sj::Value::Object(__root)
    }}
}

/// Emits one property: base schema from the (option-unwrapped) field type, then
/// merges constraint keywords + description, then inserts under the wire name.
fn named_property(field: &Field) -> TokenStream {
    let sj = sj();
    let label = crate::ir::wire_name(&field.label);
    let base_ty = option_inner(&field.ty).unwrap_or(&field.ty);
    let constraints = field.rules.iter().map(rule_tokens);
    let description = field.description.as_ref().map(|d| {
        quote! { __o.insert(::std::string::String::from("description"), #sj::Value::String(::std::string::String::from(#d))); }
    });
    quote! {{
        let mut __s = <#base_ty as ::stakit_model::JsonSchema>::schema();
        if let ::core::option::Option::Some(__o) = __s.as_object_mut() {
            #(#constraints)*
            #description
        }
        __props.insert(::std::string::String::from(#label), __s);
    }}
}

/// `{ "type": "array", "prefixItems": [...], "minItems": N, "maxItems": N }`.
fn tuple_schema(fields: &[Field]) -> TokenStream {
    let sj = sj();
    let items = fields.iter().map(|f| {
        let ty = option_inner(&f.ty).unwrap_or(&f.ty);
        quote! { __items.push(<#ty as ::stakit_model::JsonSchema>::schema()); }
    });
    let len = fields.len();
    quote! {{
        let mut __items: ::std::vec::Vec<#sj::Value> = ::std::vec::Vec::new();
        #(#items)*
        #sj::json!({ "type": "array", "prefixItems": __items, "minItems": #len, "maxItems": #len })
    }}
}

/// Enum mapping: all-unit → `{"type":"string","enum":[…]}`; otherwise a
/// `oneOf` of serde-externally-tagged variant schemas.
fn enum_schema(variants: &[Variant]) -> TokenStream {
    let sj = sj();
    if variants.is_empty() {
        return quote! { #sj::json!({ "not": {} }) };
    }
    if variants.iter().all(|v| matches!(v.body, Body::Unit)) {
        let names = variants.iter().map(|v| v.ident.to_string());
        return quote! { #sj::json!({ "type": "string", "enum": [ #(#names),* ] }) };
    }
    let arms = variants.iter().map(variant_schema);
    quote! {{
        let mut __variants: ::std::vec::Vec<#sj::Value> = ::std::vec::Vec::new();
        #(#arms)*
        #sj::json!({ "oneOf": __variants })
    }}
}

fn variant_schema(variant: &Variant) -> TokenStream {
    let sj = sj();
    let name = variant.ident.to_string();
    match &variant.body {
        Body::Unit => quote! {
            __variants.push(#sj::json!({ "type": "string", "const": #name }));
        },
        Body::Named(fields) => {
            let inner = object_schema(fields);
            quote! {{
                let __inner = #inner;
                __variants.push(#sj::json!({
                    "type": "object",
                    "properties": { #name: __inner },
                    "required": [ #name ]
                }));
            }}
        }
        Body::Tuple(fields) => {
            let inner = if fields.len() == 1 {
                let ty = option_inner(&fields[0].ty).unwrap_or(&fields[0].ty);
                quote! { <#ty as ::stakit_model::JsonSchema>::schema() }
            } else {
                tuple_schema(fields)
            };
            quote! {{
                let __inner = #inner;
                __variants.push(#sj::json!({
                    "type": "object",
                    "properties": { #name: __inner },
                    "required": [ #name ]
                }));
            }}
        }
    }
}

fn is_optional(field: &Field) -> bool {
    option_inner(&field.ty).is_some()
}

/// Lowers a `#[validate(...)]` rule onto the mutable property object `__o`.
fn rule_tokens(rule: &Rule) -> TokenStream {
    let sj = sj();
    let num = |e: &syn::Expr| quote! { #sj::json!(#e) };
    match rule {
        Rule::Length { min, max } => {
            // String vs array keyword chosen at runtime from the base "type".
            let set = |key_len: &str, key_items: &str, e: &syn::Expr| {
                let val = num(e);
                quote! {{
                    let __v = #val;
                    let __arr = __o.get("type").and_then(|__t| __t.as_str())
                        == ::core::option::Option::Some("array");
                    let __key = if __arr { #key_items } else { #key_len };
                    __o.insert(::std::string::String::from(__key), __v);
                }}
            };
            let min_t = min.as_ref().map(|e| set("minLength", "minItems", e));
            let max_t = max.as_ref().map(|e| set("maxLength", "maxItems", e));
            quote! { #min_t #max_t }
        }
        Rule::Range { min, max } => {
            let min_t = min.as_ref().map(|e| {
                let v = num(e);
                quote! { __o.insert(::std::string::String::from("minimum"), #v); }
            });
            let max_t = max.as_ref().map(|e| {
                let v = num(e);
                quote! { __o.insert(::std::string::String::from("maximum"), #v); }
            });
            quote! { #min_t #max_t }
        }
        Rule::Pattern(lit) => quote! {
            __o.insert(::std::string::String::from("pattern"), #sj::Value::String(::std::string::String::from(#lit)));
        },
        Rule::Email => quote! {
            __o.insert(::std::string::String::from("format"), #sj::Value::String(::std::string::String::from("email")));
        },
        Rule::Url => quote! {
            __o.insert(::std::string::String::from("format"), #sj::Value::String(::std::string::String::from("uri")));
        },
        // No clean schema analogue — still enforced by `Validate` at runtime.
        Rule::Ascii
        | Rule::Alphanumeric
        | Rule::Dive
        | Rule::Contains(_)
        | Rule::Prefix(_)
        | Rule::Suffix(_)
        | Rule::Custom(_) => quote! {},
    }
}
