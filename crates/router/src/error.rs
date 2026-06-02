//! The router error type.

use std::fmt;

use indexmap::IndexMap;
use stakit_model::ValidationErrors;

/// An error produced while dispatching or running an action.
#[derive(Debug)]
pub struct Error {
    /// Numeric status code (HTTP-aligned: 404, 400, 422, 500…).
    pub code: u16,
    /// Human-readable message.
    pub message: String,
    /// Per-field validation messages, when this is a validation error.
    pub fields: Option<IndexMap<String, Vec<String>>>,
}

impl Error {
    /// Unknown action name (404).
    #[must_use]
    pub fn not_found(action: &str) -> Self {
        Self {
            code: 404,
            message: format!("unknown action `{action}`"),
            fields: None,
        }
    }

    /// Parameters could not be deserialized (400).
    #[must_use]
    pub fn decode(error: &serde_json::Error) -> Self {
        Self {
            code: 400,
            message: format!("invalid parameters: {error}"),
            fields: None,
        }
    }

    /// Output could not be serialized (500).
    #[must_use]
    pub fn encode(error: &serde_json::Error) -> Self {
        Self {
            code: 500,
            message: format!("failed to serialize result: {error}"),
            fields: None,
        }
    }

    /// Validation failed (422), carrying per-field messages.
    #[must_use]
    pub fn validation(errors: ValidationErrors) -> Self {
        let mut fields: IndexMap<String, Vec<String>> = IndexMap::new();
        for error in errors {
            fields
                .entry(error.path)
                .or_default()
                .push(error.message.into_owned());
        }
        Self {
            code: 422,
            message: "validation failed".to_owned(),
            fields: Some(fields),
        }
    }

    /// An application error with an explicit code.
    #[must_use]
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fields: None,
        }
    }

    /// A generic internal error (500) from any displayable error.
    #[must_use]
    pub fn internal(error: impl fmt::Display) -> Self {
        Self {
            code: 500,
            message: error.to_string(),
            fields: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

// Any standard error an action `?`-propagates becomes a 500 — so actions can
// return *their own* error type and the router just carries it. `Error`
// deliberately does **not** implement `std::error::Error`, so this blanket can
// exist without colliding with the reflexive `From<Error>`.
impl<E> From<E> for Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(error: E) -> Self {
        Self {
            code: 500,
            message: error.to_string(),
            fields: None,
        }
    }
}

/// Builds an [`Error`]: `err!(code, msg)` or `err!(msg)` (defaults to 500). The
/// message is anything `Display`.
#[macro_export]
macro_rules! err {
    ($code:expr, $msg:expr $(,)?) => {
        $crate::Error::new($code, ::std::string::ToString::to_string(&$msg))
    };
    ($msg:expr $(,)?) => {
        $crate::Error::new(500, ::std::string::ToString::to_string(&$msg))
    };
}
