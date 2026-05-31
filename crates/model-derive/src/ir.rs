//! Parsed intermediate representation of a `#[derive(Model)]` input.

use quote::format_ident;
use syn::{Attribute, Data, DeriveInput, Expr, Fields, Ident, LitStr, Type};

/// A single validation rule attached to a field.
pub(crate) enum Rule {
    Email,
    Url,
    Ascii,
    Alphanumeric,
    Dive,
    /// Character-count length (`min_len` / `max_len`).
    Length {
        min: Option<Expr>,
        max: Option<Expr>,
    },
    /// Numeric range (`min` / `max`).
    Range {
        min: Option<Expr>,
        max: Option<Expr>,
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

/// Parses the flat `#[validate(...)]` attributes on a field.
fn parse_rules(attrs: &[Attribute]) -> syn::Result<(Vec<Rule>, bool)> {
    let mut rules = Vec::new();
    let mut skip = false;
    let (mut min_len, mut max_len, mut min, mut max) = (None, None, None, None);

    for attr in attrs {
        if !attr.path().is_ident("validate") {
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
                "min_len" => min_len = Some(meta.value()?.parse()?),
                "max_len" => max_len = Some(meta.value()?.parse()?),
                "min" => min = Some(meta.value()?.parse()?),
                "max" => max = Some(meta.value()?.parse()?),
                "contains" => rules.push(Rule::Contains(meta.value()?.parse()?)),
                "prefix" => rules.push(Rule::Prefix(meta.value()?.parse()?)),
                "suffix" => rules.push(Rule::Suffix(meta.value()?.parse()?)),
                "pattern" => rules.push(Rule::Pattern(meta.value()?.parse()?)),
                "custom" => rules.push(Rule::Custom(meta.value()?.parse()?)),
                other => {
                    return Err(meta.error(format!("unsupported `#[validate]` rule `{other}`")));
                }
            }
            Ok(())
        })?;
    }

    if min_len.is_some() || max_len.is_some() {
        rules.push(Rule::Length {
            min: min_len,
            max: max_len,
        });
    }
    if min.is_some() || max.is_some() {
        rules.push(Rule::Range { min, max });
    }
    Ok((rules, skip))
}
