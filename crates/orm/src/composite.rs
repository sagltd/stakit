//! Postgres composite-type text codec, used by `#[derive(Type)]`.
//!
//! A composite value binds as the Postgres composite **text literal** `(a,b,c)`
//! (with the standard quoting/escaping), wrapped in a [`Value::Cast`] so the SQL
//! writer appends the `::<type_name>` cast. It reads back from the same text form
//! (select the column as `col::text`). These helpers are `#[doc(hidden)]` building
//! blocks the derive emits calls to; they are not meant to be called by hand.

use crate::error::{Error, Result};
use crate::value::{Value, ValueKind};
use core::fmt::Write as _;

/// Encode composite `fields` into the Postgres text literal `(f1,f2,…)`.
///
/// Each field is rendered from its [`Value`]; `NULL` is an empty (unquoted) field,
/// and text fields are double-quoted with `"`→`""` / `\`→`\\` escaping when they
/// contain a delimiter, quote, backslash, whitespace, or are empty.
#[doc(hidden)]
#[must_use]
pub fn encode(fields: &[Value]) -> String {
    let mut out = String::with_capacity(2 + fields.len() * 8);
    out.push('(');
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        encode_field(&mut out, field);
    }
    out.push(')');
    out
}

fn encode_field(out: &mut String, value: &Value) {
    match value {
        // An empty unquoted field is SQL NULL.
        Value::Null(_) => {}
        Value::Bool(b) => out.push(if *b { 't' } else { 'f' }),
        Value::I16(n) => {
            let _ = write!(out, "{n}");
        }
        Value::I32(n) => {
            let _ = write!(out, "{n}");
        }
        Value::I64(n) => {
            let _ = write!(out, "{n}");
        }
        Value::F32(n) => {
            let _ = write!(out, "{n}");
        }
        Value::F64(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Uuid(u) => {
            let _ = write!(out, "{u}");
        }
        Value::Text(s) => encode_text(out, s),
        // Anything else (dates, json, …) renders via its Debug-free text form.
        other => encode_text(out, &scalar_text(other)),
    }
}

/// Render a scalar `Value` as the plain text Postgres would accept inside a
/// composite (no quoting — `encode_text` adds quoting if needed).
fn scalar_text(value: &Value) -> String {
    match value {
        Value::Timestamptz(t) => t.to_rfc3339(),
        Value::NaiveDateTime(t) => t.to_string(),
        Value::Date(d) => d.to_string(),
        Value::NaiveTime(t) => t.to_string(),
        Value::Json(j) => j.to_string(),
        _ => String::new(),
    }
}

fn encode_text(out: &mut String, text: &str) {
    let needs_quote = text.is_empty()
        || text
            .chars()
            .any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\') || c.is_whitespace());
    if !needs_quote {
        out.push_str(text);
        return;
    }
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push(c); // double the quote/backslash
        }
        out.push(c);
    }
    out.push('"');
}

/// Parse a Postgres composite text literal `(a,b,c)` into `expected` fields.
///
/// Each field is `None` for SQL `NULL` (an empty unquoted field) or `Some(text)`
/// with quoting/escaping undone. An empty *quoted* field (`""`) is `Some("")`.
///
/// # Errors
/// Returns [`Error::Decode`] if the parens are missing or the field count differs
/// from `expected`.
#[doc(hidden)]
pub fn parse(text: &str, expected: usize) -> Result<Vec<Option<String>>> {
    let inner = text
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| Error::Decode("composite: missing surrounding parentheses".into()))?;

    let mut fields: Vec<Option<String>> = Vec::with_capacity(expected);
    let mut buffer = String::new();
    let mut in_quote = false;
    let mut was_quoted = false;
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quote {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        buffer.push('"');
                    } else {
                        in_quote = false;
                    }
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        buffer.push(next);
                    }
                }
                other => buffer.push(other),
            }
        } else {
            match c {
                ',' => {
                    fields.push(finish_field(&buffer, was_quoted));
                    buffer.clear();
                    was_quoted = false;
                }
                '"' => {
                    in_quote = true;
                    was_quoted = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        buffer.push(next);
                    }
                }
                other => buffer.push(other),
            }
        }
    }
    // The trailing field (composites always have ≥1 field, so push unconditionally).
    fields.push(finish_field(&buffer, was_quoted));

    if fields.len() != expected {
        return Err(Error::Decode(
            format!(
                "composite: expected {expected} fields, got {}",
                fields.len()
            )
            .into(),
        ));
    }
    Ok(fields)
}

/// An unquoted empty field is NULL; anything else (incl. an empty *quoted* field)
/// is the captured text.
fn finish_field(buffer: &str, was_quoted: bool) -> Option<String> {
    if !was_quoted && buffer.is_empty() {
        None
    } else {
        Some(buffer.to_owned())
    }
}

/// Turn one parsed composite field (`None` = NULL) into a typed [`Value`] of `kind`,
/// ready for [`crate::FromValue::from_value`].
///
/// # Errors
/// Returns [`Error::Decode`] if a non-null field fails to parse as `kind`.
#[doc(hidden)]
pub fn field_value(field: &Option<String>, kind: ValueKind) -> Result<Value> {
    let Some(text) = field else {
        return Ok(Value::Null(kind));
    };
    let decode = |error: &str| Error::Decode(format!("composite field: {error}").into());
    let value = match kind {
        ValueKind::Bool => Value::Bool(matches!(text.as_str(), "t" | "true" | "T" | "1")),
        ValueKind::I16 => Value::I16(text.parse().map_err(|_| decode("bad i16"))?),
        ValueKind::I32 => Value::I32(text.parse().map_err(|_| decode("bad i32"))?),
        ValueKind::I64 => Value::I64(text.parse().map_err(|_| decode("bad i64"))?),
        ValueKind::F32 => Value::F32(text.parse().map_err(|_| decode("bad f32"))?),
        ValueKind::F64 => Value::F64(text.parse().map_err(|_| decode("bad f64"))?),
        ValueKind::Uuid => Value::Uuid(text.parse().map_err(|_| decode("bad uuid"))?),
        ValueKind::Json => Value::Json(serde_json::from_str(text).map_err(|_| decode("bad json"))?),
        // Text and everything else (dates, nested composites) are handed back as
        // text for the field type's own `FromValue` to interpret.
        _ => Value::Text(text.clone()),
    };
    Ok(value)
}

#[cfg(test)]
#[path = "composite/composite_test.rs"]
mod composite_test;
