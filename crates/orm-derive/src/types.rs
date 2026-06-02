//! Rust-type → Postgres-type mapping and type-shape detection for the derive.

use syn::{GenericArgument, PathArguments, Type};

/// If `ty` is `Wrapper<Inner>` for the given `wrapper` ident, return `Inner`.
pub(crate) fn unwrap_generic<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != wrapper {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

/// Whether `ty` is a relation marker `Rel<...>`.
pub(crate) fn is_relation(ty: &Type) -> bool {
    unwrap_generic(ty, "Rel").is_some()
}

/// The trailing identifier of a path type (e.g. `Uuid` from `uuid::Uuid`).
fn last_ident(ty: &Type) -> Option<String> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    Some(type_path.path.segments.last()?.ident.to_string())
}

/// Map a (non-`Option`) Rust type spelling to its canonical Postgres type.
///
/// Returns `None` for unknown spellings — the caller must then require an
/// explicit `#[column(sql_type = "...")]`.
pub(crate) fn sql_type(ty: &Type) -> Option<&'static str> {
    if let Type::Path(path) = ty {
        // Vec<u8> -> bytea, special-cased before the generic Vec rule.
        if let Some(inner) = unwrap_generic(ty, "Vec") {
            if last_ident(inner).as_deref() == Some("u8") {
                return Some("bytea");
            }
            return None; // other arrays need an explicit sql_type for now
        }
        let _ = path;
    }
    let name = last_ident(ty)?;
    let mapped = match name.as_str() {
        "i16" => "smallint",
        "i32" => "int",
        "i64" => "bigint",
        "f32" => "real",
        "f64" => "double precision",
        "bool" => "boolean",
        "String" | "str" => "text",
        "Uuid" => "uuid",
        "DateTime" => "timestamptz",
        "NaiveDateTime" => "timestamp",
        "NaiveDate" => "date",
        "NaiveTime" => "time",
        "Value" => "jsonb",
        _ => return None,
    };
    Some(mapped)
}
