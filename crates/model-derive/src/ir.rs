//! Parsed intermediate representation of a `#[derive(Model)]` input.

use quote::format_ident;
use syn::meta::ParseNestedMeta;
use syn::{Attribute, Data, DeriveInput, Expr, Fields, Ident, LitStr, Type};

/// A single garde validation rule attached to a field.
pub(crate) enum Rule {
    Email,
    Url,
    Ascii,
    Alphanumeric,
    Dive,
    Length {
        min: Option<Expr>,
        max: Option<Expr>,
        equal: Option<Expr>,
    },
    Range {
        min: Option<Expr>,
        max: Option<Expr>,
        equal: Option<Expr>,
    },
    Contains(Expr),
    Prefix(Expr),
    Suffix(Expr),
    Pattern(LitStr),
    Custom(Expr),
}

/// A field of a struct or an enum variant.
pub(crate) struct Field {
    /// Local binding identifier used after destructuring (the field name for
    /// named fields, `__field_N` for tuple positions).
    pub(crate) binding: Ident,
    /// Field name used for TypeScript + the validation path.
    pub(crate) label: String,
    pub(crate) ty: Type,
    pub(crate) rules: Vec<Rule>,
    pub(crate) skip: bool,
}

/// Shape of a struct or of an enum variant's payload.
pub(crate) enum Body {
    Unit,
    Named(Vec<Field>),
    Tuple(Vec<Field>),
}

/// An enum variant.
pub(crate) struct Variant {
    pub(crate) ident: Ident,
    pub(crate) body: Body,
}

/// Parsed model input.
pub(crate) enum Ir {
    Struct { body: Body },
    Enum { variants: Vec<Variant> },
}

/// Parses a [`DeriveInput`] into the [`Ir`] plus the type ident.
pub(crate) fn parse(input: &DeriveInput) -> syn::Result<(Ident, Ir)> {
    let ident = input.ident.clone();
    let ir = match &input.data {
        Data::Struct(data) => Ir::Struct {
            body: parse_fields(&data.fields)?,
        },
        Data::Enum(data) => {
            let mut variants = Vec::with_capacity(data.variants.len());
            for v in &data.variants {
                variants.push(Variant {
                    ident: v.ident.clone(),
                    body: parse_fields(&v.fields)?,
                });
            }
            Ir::Enum { variants }
        }
        Data::Union(u) => {
            return Err(syn::Error::new(
                u.union_token.span,
                "`#[derive(Model)]` does not support unions",
            ));
        }
    };
    Ok((ident, ir))
}

fn parse_fields(fields: &Fields) -> syn::Result<Body> {
    match fields {
        Fields::Unit => Ok(Body::Unit),
        Fields::Named(named) => {
            let mut out = Vec::with_capacity(named.named.len());
            for f in &named.named {
                let ident = f.ident.clone().expect("named field has ident");
                let (rules, skip) = parse_rules(&f.attrs)?;
                out.push(Field {
                    label: ident.to_string(),
                    binding: ident,
                    ty: f.ty.clone(),
                    rules,
                    skip,
                });
            }
            Ok(Body::Named(out))
        }
        Fields::Unnamed(unnamed) => {
            let mut out = Vec::with_capacity(unnamed.unnamed.len());
            for (i, f) in unnamed.unnamed.iter().enumerate() {
                let (rules, skip) = parse_rules(&f.attrs)?;
                out.push(Field {
                    binding: format_ident!("__field_{i}"),
                    label: i.to_string(),
                    ty: f.ty.clone(),
                    rules,
                    skip,
                });
            }
            Ok(Body::Tuple(out))
        }
    }
}

fn parse_rules(attrs: &[Attribute]) -> syn::Result<(Vec<Rule>, bool)> {
    let mut rules = Vec::new();
    let mut skip = false;
    for attr in attrs {
        if !attr.path().is_ident("garde") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match name.as_str() {
                "skip" => skip = true,
                "email" => rules.push(Rule::Email),
                "url" => rules.push(Rule::Url),
                "ascii" => rules.push(Rule::Ascii),
                "alphanumeric" => rules.push(Rule::Alphanumeric),
                "dive" => rules.push(Rule::Dive),
                "length" => {
                    let (min, max, equal) = parse_bounds(&meta)?;
                    rules.push(Rule::Length { min, max, equal });
                }
                "range" => {
                    let (min, max, equal) = parse_bounds(&meta)?;
                    rules.push(Rule::Range { min, max, equal });
                }
                "contains" => rules.push(Rule::Contains(parse_paren_expr(&meta)?)),
                "prefix" => rules.push(Rule::Prefix(parse_paren_expr(&meta)?)),
                "suffix" => rules.push(Rule::Suffix(parse_paren_expr(&meta)?)),
                "pattern" => rules.push(Rule::Pattern(parse_paren_litstr(&meta)?)),
                "custom" => rules.push(Rule::Custom(parse_paren_expr(&meta)?)),
                other => {
                    return Err(meta.error(format!(
                        "unsupported garde rule `{other}` in stakit-model v1"
                    )));
                }
            }
            Ok(())
        })?;
    }
    Ok((rules, skip))
}

fn parse_bounds(
    meta: &ParseNestedMeta<'_>,
) -> syn::Result<(Option<Expr>, Option<Expr>, Option<Expr>)> {
    let (mut min, mut max, mut equal) = (None, None, None);
    meta.parse_nested_meta(|m| {
        if m.path.is_ident("min") {
            min = Some(m.value()?.parse()?);
        } else if m.path.is_ident("max") {
            max = Some(m.value()?.parse()?);
        } else if m.path.is_ident("equal") {
            equal = Some(m.value()?.parse()?);
        } else {
            return Err(m.error("expected `min`, `max`, or `equal`"));
        }
        Ok(())
    })?;
    Ok((min, max, equal))
}

fn parse_paren_expr(meta: &ParseNestedMeta<'_>) -> syn::Result<Expr> {
    let content;
    syn::parenthesized!(content in meta.input);
    content.parse()
}

fn parse_paren_litstr(meta: &ParseNestedMeta<'_>) -> syn::Result<LitStr> {
    let content;
    syn::parenthesized!(content in meta.input);
    content.parse()
}
