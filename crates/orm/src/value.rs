//! Backend-neutral values.
//!
//! The query builder produces [`Value`]s for binds and decodes rows through
//! [`FromValue`], so the core never names a concrete sqlx `Database` type. Each
//! [`crate::driver::Driver`] translates `Value` ⇄ its native parameter/row types,
//! which is what lets one ORM run on Postgres, `SQLite`, `MySQL`, and Turso.

use crate::error::{Error, Result};
use core::hash::{BuildHasher, Hash};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap};

/// Re-exported scalar types so callers/drivers share one definition.
pub use sqlx::types::Uuid;
pub use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

/// A backend-neutral, owned SQL value (bind input or decoded cell).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`, carrying the column's scalar kind so drivers can bind a
    /// correctly-typed null.
    Null(ValueKind),
    /// `smallint`.
    I16(i16),
    /// `int`.
    I32(i32),
    /// `bigint`.
    I64(i64),
    /// `real`.
    F32(f32),
    /// `double precision`.
    F64(f64),
    /// `boolean`.
    Bool(bool),
    /// `text`.
    Text(String),
    /// `bytea`/`blob`.
    Bytes(Vec<u8>),
    /// `uuid`.
    Uuid(Uuid),
    /// `timestamptz` — an absolute instant ([`DateTime<Utc>`]).
    Timestamptz(DateTime<Utc>),
    /// `timestamp` (no time zone) — a naive wall-clock datetime.
    NaiveDateTime(NaiveDateTime),
    /// `date` — a calendar date.
    Date(NaiveDate),
    /// `time` — a wall-clock time of day.
    NaiveTime(NaiveTime),
    /// `json`/`jsonb` — an arbitrary JSON document.
    Json(serde_json::Value),
    /// An embedding vector (`f32` components) for vector search. Bound as the
    /// backend's vector literal (`$N::vector` on pgvector, `vector32($N)` on Turso,
    /// JSON text on `sqlite-vec`); see [`crate::vector`].
    Vector(Vec<f32>),
    /// A `PostGIS` geometry/geography: bare WKT (e.g. `POINT(1 2)`, **no** SRID
    /// prefix) plus an optional SRID as a first-class field. Bound as
    /// `$N::geometry` (or `ST_SetSRID($N::geometry, srid)` when `srid` is set) on
    /// Postgres, and read back via `ST_AsText`; see [`crate::geo`].
    Geo {
        /// Bare geometry well-known-text (no `SRID=..;` prefix).
        wkt: String,
        /// Spatial reference id, applied at bind time when present.
        srid: Option<i32>,
    },
    /// A homogeneous array of scalar values (for `= ANY($1)` / `IN`), carrying
    /// the element kind so an empty array still binds with a concrete type.
    Array(ValueKind, Vec<Self>),
    /// A value bound with an explicit SQL cast: the placeholder is rendered
    /// `$N::<cast>` (Postgres). The `inner` value carries the actual bind payload
    /// (usually [`Value::Text`]); the cast names a DB type the placeholder must be
    /// coerced to — a Postgres composite ([`derive@crate::Type`]), enum, domain, or
    /// any user/extension type. On dialects without `::` casts the bind is rejected
    /// at build time with [`Error::Unsupported`]. This is the user-extensibility
    /// hook: any `ToValue` impl may return it to request a native cast.
    Cast {
        /// The DB type name to cast the placeholder to (developer-trusted, never
        /// user input — emitted verbatim after `::`).
        cast: &'static str,
        /// The actual bound payload (e.g. the composite text literal as `Text`).
        inner: Box<Self>,
    },
}

/// The scalar kind a column decodes as — the hint a [`Row`](crate::driver::Row)
/// uses to pull the right native type out of a result cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `smallint`.
    I16,
    /// `int`.
    I32,
    /// `bigint`.
    I64,
    /// `real`.
    F32,
    /// `double precision`.
    F64,
    /// `boolean`.
    Bool,
    /// `text`.
    Text,
    /// `bytea`/`blob`.
    Bytes,
    /// `uuid`.
    Uuid,
    /// `timestamptz` (absolute instant).
    Timestamptz,
    /// `timestamp` without time zone (naive datetime).
    NaiveDateTime,
    /// `date`.
    Date,
    /// `time` of day.
    NaiveTime,
    /// `json`/`jsonb`.
    Json,
    /// An embedding vector for vector search.
    Vector,
    /// A `PostGIS` geometry/geography (carried as (E)WKT text).
    Geo,
    /// An array column decoding into `Vec<T>`, carrying the element kind so the
    /// driver knows how to read it (native `int[]`/`text[]` on Postgres, JSON text
    /// elsewhere). The `&'static` reference is a const-promoted [`FromValue::KIND`].
    Array(&'static Self),
}

fn mismatch(expected: &str, got: &Value) -> Error {
    Error::Decode(format!("expected {expected}, got {got:?}").into())
}

/// A Rust value that can be bound as a [`Value`].
pub trait ToValue {
    /// A SQL cast to apply to this type's bound placeholder (`$N::<cast>`), e.g.
    /// `Some("address_type")` for a Postgres composite or a native enum/domain.
    /// Applied at the bind boundary so it also casts `NULL` (`Option::None`), which
    /// a [`Value::Cast`] in [`to_value`](Self::to_value) cannot. `None` (default)
    /// binds the placeholder bare. Only takes effect on cast-capable dialects.
    const WRITE_CAST: Option<&'static str> = None;
    /// Convert into a backend-neutral value.
    fn to_value(self) -> Value;
}

/// Wrap `value` in a [`Value::Cast`] when `cast` is set (placeholder renders `$N::<cast>`).
///
/// The bind boundary ([`crate::insert::boxed_bind`], `IntoExpr`) uses this to apply a
/// type's [`ToValue::WRITE_CAST`] uniformly to values and `NULL`s.
#[must_use]
pub fn with_cast(value: Value, cast: Option<&'static str>) -> Value {
    match cast {
        Some(cast) => Value::Cast {
            cast,
            inner: Box::new(value),
        },
        None => value,
    }
}

/// A Rust type that can be decoded from a [`Value`].
pub trait FromValue: Sized {
    /// The scalar kind to extract from a result cell.
    const KIND: ValueKind;
    /// A SQL cast to apply when **selecting** this type, so the cell arrives in a
    /// form [`from_value`](Self::from_value) can decode. `Some("text")` for a
    /// Postgres composite (read as `col::text`); `None` (the default) selects the
    /// column bare. Only applied on dialects that support `::` casts.
    const READ_CAST: Option<&'static str> = None;
    /// Convert a decoded cell into this type.
    ///
    /// # Errors
    /// Returns [`Error::Decode`] on a type mismatch or unexpected `NULL`.
    fn from_value(value: Value) -> Result<Self>;
}

/// Generate `ToValue`/`FromValue` for a scalar mapped to one `Value` variant.
macro_rules! scalar_value {
    ($ty:ty, $variant:ident, $kind:ident, $label:literal) => {
        impl ToValue for $ty {
            fn to_value(self) -> Value {
                Value::$variant(self)
            }
        }
        impl FromValue for $ty {
            const KIND: ValueKind = ValueKind::$kind;
            fn from_value(value: Value) -> Result<Self> {
                match value {
                    Value::$variant(inner) => Ok(inner),
                    other => Err(mismatch($label, &other)),
                }
            }
        }
    };
}

scalar_value!(i16, I16, I16, "i16");
scalar_value!(i32, I32, I32, "i32");
scalar_value!(i64, I64, I64, "i64");
scalar_value!(f32, F32, F32, "f32");
scalar_value!(f64, F64, F64, "f64");
scalar_value!(bool, Bool, Bool, "bool");
scalar_value!(String, Text, Text, "text");
scalar_value!(Vec<u8>, Bytes, Bytes, "bytes");
scalar_value!(Uuid, Uuid, Uuid, "uuid");
scalar_value!(DateTime<Utc>, Timestamptz, Timestamptz, "timestamptz");
scalar_value!(NaiveDateTime, NaiveDateTime, NaiveDateTime, "timestamp");
scalar_value!(NaiveDate, Date, Date, "date");
scalar_value!(NaiveTime, NaiveTime, NaiveTime, "time");
scalar_value!(serde_json::Value, Json, Json, "json");

impl ToValue for &str {
    fn to_value(self) -> Value {
        Value::Text(self.to_owned())
    }
}

impl<T: ToValue + FromValue> ToValue for Option<T> {
    const WRITE_CAST: Option<&'static str> = T::WRITE_CAST;
    fn to_value(self) -> Value {
        self.map_or(Value::Null(T::KIND), ToValue::to_value)
    }
}

impl<T: ToValue + FromValue> ToValue for Vec<T> {
    fn to_value(self) -> Value {
        Value::Array(T::KIND, self.into_iter().map(ToValue::to_value).collect())
    }
}

impl<T: FromValue> FromValue for Option<T> {
    const KIND: ValueKind = T::KIND;
    const READ_CAST: Option<&'static str> = T::READ_CAST;
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Null(_) => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

/// Convert a scalar [`Value`] to JSON, for the array JSON fallback on backends
/// without native arrays. Non-scalar/nested values map to JSON `null`.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn scalar_to_json(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::I16(n) => J::from(*n),
        Value::I32(n) => J::from(*n),
        Value::I64(n) => J::from(*n),
        Value::F32(n) => J::from(f64::from(*n)),
        Value::F64(n) => serde_json::Number::from_f64(*n).map_or(J::Null, J::Number),
        Value::Bool(b) => J::from(*b),
        Value::Text(s) => J::from(s.clone()),
        Value::Uuid(u) => J::from(u.to_string()),
        Value::Timestamptz(t) => J::from(t.to_rfc3339()),
        Value::NaiveDateTime(t) => J::from(t.to_string()),
        Value::Date(d) => J::from(d.to_string()),
        Value::NaiveTime(t) => J::from(t.to_string()),
        Value::Json(j) => j.clone(),
        Value::Null(_)
        | Value::Bytes(_)
        | Value::Vector(_)
        | Value::Geo { .. }
        | Value::Array(..)
        | Value::Cast { .. } => J::Null,
    }
}

/// Convert a JSON scalar back into a [`Value`] of `kind` (array JSON fallback read).
#[allow(clippy::cast_possible_truncation)]
fn json_to_scalar(json: &serde_json::Value, kind: ValueKind) -> Result<Value> {
    if json.is_null() {
        return Ok(Value::Null(kind));
    }
    let bad = |what: &str| Error::Decode(format!("array element: expected {what}").into());
    let value = match kind {
        ValueKind::I16 => Value::I16(
            json.as_i64()
                .and_then(|n| i16::try_from(n).ok())
                .ok_or_else(|| bad("i16"))?,
        ),
        ValueKind::I32 => Value::I32(
            json.as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| bad("i32"))?,
        ),
        ValueKind::I64 => Value::I64(json.as_i64().ok_or_else(|| bad("i64"))?),
        ValueKind::F32 => Value::F32(json.as_f64().ok_or_else(|| bad("f32"))? as f32),
        ValueKind::F64 => Value::F64(json.as_f64().ok_or_else(|| bad("f64"))?),
        ValueKind::Bool => Value::Bool(json.as_bool().ok_or_else(|| bad("bool"))?),
        ValueKind::Uuid => Value::Uuid(
            json.as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad("uuid"))?,
        ),
        // Date/time elements were stored as strings by `scalar_to_json`; parse them
        // back into the typed value (mirrors the Turso text decode).
        ValueKind::Timestamptz => Value::Timestamptz(
            json.as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .ok_or_else(|| bad("timestamptz"))?,
        ),
        ValueKind::NaiveDateTime => Value::NaiveDateTime(
            json.as_str()
                .and_then(|s| {
                    // `NaiveDateTime::to_string()` is space-separated; `FromStr` wants
                    // `T` — accept both.
                    s.parse()
                        .ok()
                        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok())
                })
                .ok_or_else(|| bad("timestamp"))?,
        ),
        ValueKind::Date => Value::Date(
            json.as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad("date"))?,
        ),
        ValueKind::NaiveTime => Value::NaiveTime(
            json.as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad("time"))?,
        ),
        // Text and everything else round-trips through text.
        _ => Value::Text(
            json.as_str()
                .map_or_else(|| json.to_string(), ToOwned::to_owned),
        ),
    };
    Ok(value)
}

/// Encode an array's items as a JSON-array text literal (non-native-array backends).
pub(crate) fn array_to_json_text(items: &[Value]) -> String {
    let array = items.iter().map(scalar_to_json).collect();
    serde_json::Value::Array(array).to_string()
}

/// Decode a JSON-array text literal into a [`Value::Array`] of `elem` kind.
///
/// # Errors
/// Returns [`Error::Decode`] if the text isn't a JSON array or an element mismatches.
pub(crate) fn json_text_to_array(text: &str, elem: ValueKind) -> Result<Value> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|error| Error::Decode(Box::new(error)))?;
    let serde_json::Value::Array(items) = json else {
        return Err(Error::Decode("expected a JSON array".into()));
    };
    let values = items
        .iter()
        .map(|item| json_to_scalar(item, elem))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(elem, values))
}

/// Deserialize any `serde` type from a JSON cell (`Value::Json`, or `Value::Text`
/// on backends that store JSON as text). Used by the map column impls.
fn from_json_cell<T: DeserializeOwned>(value: Value) -> Result<T> {
    let json = match value {
        Value::Json(json) => json,
        Value::Text(text) => {
            serde_json::from_str(&text).map_err(|error| Error::Decode(Box::new(error)))?
        }
        other => return Err(mismatch("json", &other)),
    };
    serde_json::from_value(json).map_err(|error| Error::Decode(Box::new(error)))
}

/// `HashMap<K, V>` stores as a JSON object (`jsonb` on PG/MySQL, text on
/// SQLite/Turso) — portable, no `hstore` extension required.
impl<K, V, S> ToValue for HashMap<K, V, S>
where
    K: Serialize + Eq + Hash,
    V: Serialize,
    S: BuildHasher,
{
    fn to_value(self) -> Value {
        Value::Json(serde_json::to_value(self).unwrap_or(serde_json::Value::Null))
    }
}

impl<K, V, S> FromValue for HashMap<K, V, S>
where
    K: DeserializeOwned + Eq + Hash,
    V: DeserializeOwned,
    S: BuildHasher + Default,
{
    const KIND: ValueKind = ValueKind::Json;
    fn from_value(value: Value) -> Result<Self> {
        from_json_cell(value)
    }
}

/// `BTreeMap<K, V>` stores as a JSON object, like [`HashMap`] but key-ordered.
impl<K, V> ToValue for BTreeMap<K, V>
where
    K: Serialize + Ord,
    V: Serialize,
{
    fn to_value(self) -> Value {
        Value::Json(serde_json::to_value(self).unwrap_or(serde_json::Value::Null))
    }
}

impl<K, V> FromValue for BTreeMap<K, V>
where
    K: DeserializeOwned + Ord,
    V: DeserializeOwned,
{
    const KIND: ValueKind = ValueKind::Json;
    fn from_value(value: Value) -> Result<Self> {
        from_json_cell(value)
    }
}

/// Read a `Vec<T>` column: a native array on Postgres (`int[]`/`text[]`/…), or a
/// JSON array elsewhere — both arrive as [`Value::Array`] of the element kind.
impl<T: ToValue + FromValue> FromValue for Vec<T> {
    // `&T::KIND` is a const reference to an associated const → promoted to 'static.
    const KIND: ValueKind = ValueKind::Array(&T::KIND);
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Array(_, items) => items.into_iter().map(T::from_value).collect(),
            Value::Null(_) => Ok(Self::new()),
            other => Err(mismatch("array", &other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FromValue, ToValue, Value, ValueKind};

    #[test]
    fn scalar_round_trips() {
        assert_eq!(i64::from_value(42_i64.to_value()).unwrap(), 42);
        assert_eq!(String::from_value("hi".to_value()).unwrap(), "hi");
        assert!(bool::from_value(true.to_value()).unwrap());
    }

    #[test]
    fn option_some_and_none() {
        assert_eq!(Some(7_i32).to_value(), Value::I32(7));
        assert_eq!(Option::<i32>::None.to_value(), Value::Null(ValueKind::I32));
        assert_eq!(
            Option::<i64>::from_value(Value::Null(ValueKind::I64)).unwrap(),
            None
        );
        assert_eq!(Option::<i64>::from_value(Value::I64(5)).unwrap(), Some(5));
    }

    #[test]
    fn non_option_rejects_null() {
        assert!(i64::from_value(Value::Null(ValueKind::I64)).is_err());
    }

    #[test]
    fn type_mismatch_is_error() {
        assert!(i64::from_value(Value::Text("x".to_owned())).is_err());
    }

    #[test]
    fn vec_becomes_array() {
        assert_eq!(
            vec![1_i32, 2, 3].to_value(),
            Value::Array(
                ValueKind::I32,
                vec![Value::I32(1), Value::I32(2), Value::I32(3)]
            )
        );
    }

    #[test]
    fn kind_matches_type() {
        assert_eq!(<i64 as FromValue>::KIND, ValueKind::I64);
        assert_eq!(<Option<String> as FromValue>::KIND, ValueKind::Text);
    }

    #[test]
    fn str_binds_as_text() {
        assert_eq!("hi".to_value(), Value::Text("hi".to_owned()));
    }
}
