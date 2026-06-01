//! The neutral reply envelope returned by [`Router::on_request`](crate::Router::on_request).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

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
    /// Human-readable message.
    pub message: String,
    /// Per-field validation messages, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<String, Vec<String>>>,
}

impl From<Error> for ErrorBody {
    fn from(error: Error) -> Self {
        Self {
            code: error.code,
            message: error.message,
            fields: error.fields,
        }
    }
}

/// One frame of a streaming response (for SSE / websocket transports).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Frame {
    /// A streamed item.
    Next {
        /// Serialized item value.
        data: Value,
    },
    /// A failure (terminates the stream).
    Error {
        /// Error details.
        error: ErrorBody,
    },
    /// End-of-stream marker.
    End,
}

impl Frame {
    /// A `Next` frame.
    #[must_use]
    pub const fn next(data: Value) -> Self {
        Self::Next { data }
    }

    /// An `Error` frame from an [`Error`].
    #[must_use]
    pub fn error(error: Error) -> Self {
        Self::Error {
            error: error.into(),
        }
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
}
