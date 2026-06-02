//! The router error type.

use std::fmt;

use indexmap::IndexMap;
use stakit_model::ValidationErrors;

/// An error produced while dispatching or running an action.
///
/// `message` is **client-facing**. `detail` is **server-side only** (never
/// serialized into a reply): an internal error that bubbles up via `?` is shown
/// to the client as a generic message, with its real text kept in `detail` for
/// logging — so a DB error / file path can't leak to callers.
#[derive(Debug)]
pub struct Error {
    /// Numeric status code (HTTP-aligned: 404, 400, 422, 500…).
    pub code: u16,
    /// Client-facing message (safe to send over the wire).
    pub message: String,
    /// Per-field validation messages, when this is a validation error. Boxed so a
    /// large validation map doesn't bloat every `Result<_, Error>`.
    pub fields: Option<Box<IndexMap<String, Vec<String>>>>,
    /// Server-side detail, never sent to clients (log it).
    pub detail: Option<String>,
}

/// Client message used for internal (`?`-propagated / 500) errors so their real
/// text doesn't leak; the original goes to [`Error::detail`].
const INTERNAL_MESSAGE: &str = "internal server error";

impl Error {
    /// Unknown action name (404).
    #[must_use]
    pub fn not_found(action: &str) -> Self {
        Self {
            code: 404,
            message: format!("unknown action `{action}`"),
            fields: None,
            detail: None,
        }
    }

    /// Parameters could not be deserialized (400).
    #[must_use]
    pub fn decode(error: &serde_json::Error) -> Self {
        Self {
            code: 400,
            message: format!("invalid parameters: {error}"),
            fields: None,
            detail: None,
        }
    }

    /// Output could not be serialized (500). The serde detail is kept server-side.
    #[must_use]
    pub fn encode(error: &serde_json::Error) -> Self {
        Self {
            code: 500,
            message: INTERNAL_MESSAGE.to_owned(),
            fields: None,
            detail: Some(format!("failed to serialize result: {error}")),
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
            fields: Some(Box::new(fields)),
            detail: None,
        }
    }

    /// An application error with an explicit, client-facing code + message.
    #[must_use]
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fields: None,
            detail: None,
        }
    }

    /// A generic internal error (500): the client sees a generic message; the
    /// real text is kept in [`Error::detail`] for server-side logging.
    #[must_use]
    pub fn internal(error: impl fmt::Display) -> Self {
        Self {
            code: 500,
            message: INTERNAL_MESSAGE.to_owned(),
            fields: None,
            detail: Some(error.to_string()),
        }
    }

    /// The server-side detail (never sent to clients), if any.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, " (detail: {detail})")?;
        }
        Ok(())
    }
}

// Any standard error an action `?`-propagates becomes a 500 — so actions can
// return *their own* error type and the router just carries it. The real text
// goes to `detail` (logged), not `message` (sent to the client), so internal
// errors don't leak. `Error` deliberately does **not** implement
// `std::error::Error`, so this blanket can't collide with the reflexive `From`.
impl<E> From<E> for Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(error: E) -> Self {
        Self::internal(error)
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
