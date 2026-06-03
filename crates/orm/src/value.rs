//! Backend-neutral values.
//!
//! The query builder produces [`Value`]s for binds and decodes rows through
//! [`FromValue`], so the core never names a concrete sqlx `Database` type. Each
//! [`crate::driver::Driver`] translates `Value` ⇄ its native parameter/row types,
//! which is what lets one ORM run on Postgres, `SQLite`, `MySQL`, and Turso.

use crate::error::{Error, Result};

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
}

fn mismatch(expected: &str, got: &Value) -> Error {
    Error::Decode(format!("expected {expected}, got {got:?}").into())
}

/// A Rust value that can be bound as a [`Value`].
pub trait ToValue {
    /// Convert into a backend-neutral value.
    fn to_value(self) -> Value;
}

/// A Rust type that can be decoded from a [`Value`].
pub trait FromValue: Sized {
    /// The scalar kind to extract from a result cell.
    const KIND: ValueKind;
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
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Null(_) => Ok(None),
            other => T::from_value(other).map(Some),
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
