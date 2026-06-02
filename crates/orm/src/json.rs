//! Typed JSON columns — store any `serde` struct in a `json`/`jsonb` (or text)
//! column via [`Json<T>`].
//!
//! ```no_run
//! use stakit_orm::prelude::*;
//! use stakit_orm::Json;
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
//! struct Profile { theme: String, notifications: bool }
//!
//! #[derive(Table)]
//! #[table(name = "users")]
//! struct User {
//!     #[column(pk)] id: i64,
//!     #[column(sql_type = "jsonb")] profile: Json<Profile>,
//! }
//! ```

use crate::error::{Error, Result};
use crate::value::{FromValue, ToValue, Value, ValueKind};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A column wrapper that serializes any `T: Serialize + Deserialize` to/from JSON,
/// so Rust structs/enums can be stored directly in a JSON column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    /// Unwrap the inner value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Json<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T: Serialize> ToValue for Json<T> {
    fn to_value(self) -> Value {
        // Serialization of an in-memory value effectively never fails for the
        // derived `Serialize` impls this targets; fall back to JSON null otherwise.
        Value::Json(serde_json::to_value(self.0).unwrap_or(serde_json::Value::Null))
    }
}

impl<T: DeserializeOwned> FromValue for Json<T> {
    const KIND: ValueKind = ValueKind::Json;
    fn from_value(value: Value) -> Result<Self> {
        let json = match value {
            Value::Json(json) => json,
            // Some backends hand JSON back as text (e.g. SQLite without the JSON
            // type) — parse it.
            Value::Text(text) => {
                serde_json::from_str(&text).map_err(|error| Error::Decode(Box::new(error)))?
            }
            other => {
                return Err(Error::Decode(
                    format!("expected json, got {other:?}").into(),
                ));
            }
        };
        serde_json::from_value(json)
            .map(Self)
            .map_err(|error| Error::Decode(Box::new(error)))
    }
}
