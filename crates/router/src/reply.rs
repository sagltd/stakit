//! The neutral reply envelope returned by [`Router::on_request`](crate::Router::on_request).

use std::borrow::Cow;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Error;

/// Default machine-readable error code; omitted from the wire when unset.
const fn default_kind() -> Cow<'static, str> {
    Cow::Borrowed("error")
}

/// Whether the machine code is the default (so it can be skipped on the wire).
fn is_default_kind(kind: &str) -> bool {
    kind == "error"
}

/// Outcome of a unary action call, ready for the framework to serialize.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Reply {
    /// Success — `data` holds the serialized action output.
    Ok {
        /// Serialized output value.
        data: Value,
    },
    /// Failure — `error` describes what went wrong.
    Error {
        /// Error details.
        error: ErrorBody,
    },
}

/// Serializable error body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Numeric status code.
    pub code: u16,
    /// Stable, machine-readable error code (wire key `type`); defaults to
    /// `"ERROR"` and is omitted from the wire when unset.
    #[serde(
        rename = "type",
        default = "default_kind",
        skip_serializing_if = "is_default_kind"
    )]
    pub kind: Cow<'static, str>,
    /// Human-readable message.
    pub message: String,
    /// Per-field validation messages, when present. Boxed so the (rare,
    /// validation-only) map doesn't bloat every `ErrorBody` / `Result<_, _>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Box<IndexMap<String, Vec<String>>>>,
}

impl From<Error> for ErrorBody {
    fn from(error: Error) -> Self {
        Self {
            code: error.code,
            kind: error.kind,
            message: error.message,
            fields: error.fields,
        }
    }
}

impl ErrorBody {
    /// Hand-builds the JSON object (skips serde reflection on the hot path).
    fn into_value(self) -> Value {
        let mut object = Map::with_capacity(4);
        object.insert("code".to_owned(), Value::from(self.code));
        if !is_default_kind(&self.kind) {
            object.insert("type".to_owned(), Value::String(self.kind.into_owned()));
        }
        object.insert("message".to_owned(), Value::String(self.message));
        if let Some(fields) = self.fields {
            object.insert(
                "fields".to_owned(),
                serde_json::to_value(fields).unwrap_or(Value::Null),
            );
        }
        Value::Object(object)
    }

    /// The machine-readable error code (the wire `type`), e.g. `"not_found"`.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Whether this is a validation error — the Rust mirror of the generated
    /// TypeScript `isValidationError` guard.
    #[must_use]
    pub fn is_validation(&self) -> bool {
        self.kind == "validation"
    }
}

/// One frame of a streaming response.
///
/// Carries `index` + `action` so a client streaming several actions over one
/// connection can demux frames (the `index` also disambiguates the same action
/// requested twice via the array payload).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Frame {
    /// A streamed item.
    Next {
        /// Position of this action in the request payload.
        index: usize,
        /// The action that produced this item.
        action: String,
        /// Serialized item value.
        data: Value,
    },
    /// A failure (terminates this action's substream).
    Error {
        /// Position of this action in the request payload.
        index: usize,
        /// The action that produced this error.
        action: String,
        /// Error details.
        error: ErrorBody,
    },
    /// End-of-stream marker for one action.
    End {
        /// Position of this action in the request payload.
        index: usize,
        /// The action that finished.
        action: String,
    },
}

impl Frame {
    /// A `Next` frame.
    #[must_use]
    pub const fn next(index: usize, action: String, data: Value) -> Self {
        Self::Next {
            index,
            action,
            data,
        }
    }

    /// An `Error` frame from an [`Error`].
    #[must_use]
    pub fn error(index: usize, action: String, error: Error) -> Self {
        Self::Error {
            index,
            action,
            error: error.into(),
        }
    }

    /// An `End` frame.
    #[must_use]
    pub const fn end(index: usize, action: String) -> Self {
        Self::End { index, action }
    }
}

impl Reply {
    /// Builds a success reply.
    #[must_use]
    pub const fn ok(data: Value) -> Self {
        Self::Ok { data }
    }

    /// Builds an error reply from an [`Error`].
    #[must_use]
    pub fn error(error: Error) -> Self {
        Self::Error {
            error: ErrorBody {
                code: error.code,
                kind: error.kind,
                message: error.message,
                fields: error.fields,
            },
        }
    }

    /// The status code (success is always 200).
    #[must_use]
    pub const fn code(&self) -> u16 {
        match self {
            Self::Ok { .. } => 200,
            Self::Error { error } => error.code,
        }
    }

    /// Hand-builds the envelope JSON object, avoiding serde reflection on the
    /// hot dispatch path.
    pub(crate) fn into_value(self) -> Value {
        let mut object = Map::with_capacity(2);
        match self {
            Self::Ok { data } => {
                object.insert("status".to_owned(), Value::String("ok".to_owned()));
                object.insert("data".to_owned(), data);
            }
            Self::Error { error } => {
                object.insert("status".to_owned(), Value::String("error".to_owned()));
                object.insert("error".to_owned(), error.into_value());
            }
        }
        Value::Object(object)
    }
}
