//! The router error type.

use std::borrow::Cow;
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
    /// Stable, machine-readable error code (e.g. `"not_found"`,
    /// `"invalid_credentials"`). Serialized on the wire as `type`; defaults to
    /// `"error"`. Lets clients branch on a code instead of parsing `message`.
    pub kind: Cow<'static, str>,
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
            kind: Cow::Borrowed("not_found"),
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
            kind: Cow::Borrowed("bad_request"),
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
            kind: Cow::Borrowed("internal"),
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
            kind: Cow::Borrowed("validation"),
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
            kind: Cow::Borrowed("error"),
            message: message.into(),
            fields: None,
            detail: None,
        }
    }

    /// An application error with an explicit status, machine-readable code, and
    /// client-facing message — the manual equivalent of deriving
    /// [`ResponseError`].
    #[must_use]
    pub fn coded(
        code: u16,
        kind: impl Into<Cow<'static, str>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            kind: kind.into(),
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
            kind: Cow::Borrowed("internal"),
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

/// An error type an action can return: it declares its own HTTP status, a
/// stable machine-readable code, and a client-facing message.
///
/// Implement it by hand, or — far more commonly — derive it alongside
/// [`thiserror::Error`], declaring the status per variant:
///
/// ```ignore
/// #[derive(Debug, thiserror::Error, stakit_router::ResponseError)]
/// pub enum ActionError {
///     // `code` defaults to the variant name in `snake_case` -> "user_not_found".
///     #[status(404)]
///     #[error("user not found")]
///     UserNotFound,
///
///     // override the default code with `#[code("...")]`.
///     #[status(401)] #[code("login_failed")]
///     #[error("invalid credentials")]
///     InvalidCredentials,
///
///     // foreign errors flow in via thiserror's `#[from]`, so `?` just works;
///     // 5xx messages are auto-genericized (real text logged, not leaked).
///     #[status(500)]
///     #[error(transparent)]
///     Db(#[from] stakit_orm::Error),
/// }
/// ```
///
/// Any `ResponseError` is `Into<Error>`, so the router converts it
/// automatically — preserving the declared status instead of collapsing every
/// error to 500. The set of codes is collected into the generated TypeScript
/// `ErrorCode` union (see [`ErrorCodes`]).
pub trait ResponseError: fmt::Display {
    /// HTTP-aligned status code for this error (404, 401, 422, 500…).
    fn status(&self) -> u16;

    /// Stable, machine-readable code (e.g. `"not_found"`). Defaults to `"error"`.
    fn code(&self) -> Cow<'static, str> {
        Cow::Borrowed("error")
    }

    /// Client-facing message. Defaults to the [`Display`](fmt::Display) text.
    ///
    /// For `status >= 500` the [`From`] conversion replaces this with a generic
    /// message and keeps the real text in [`Error::detail`] (logged, never
    /// leaked), so a server error can't expose internals to the caller.
    fn message(&self) -> Cow<'_, str> {
        Cow::Owned(self.to_string())
    }
}

// Any `ResponseError` converts into the router's `Error`, carrying its declared
// status + code. Bounded on *our* marker trait (not `std::error::Error`), so it
// neither maps everything to 500 nor blocks user-written `From` impls. A 5xx
// message is genericized here (single place), with the real text moved to
// `detail` for logging.
impl<E: ResponseError> From<E> for Error {
    fn from(error: E) -> Self {
        let code = error.status();
        if code >= 500 {
            return Self {
                code,
                kind: error.code(),
                message: INTERNAL_MESSAGE.to_owned(),
                fields: None,
                detail: Some(error.to_string()),
            };
        }
        Self {
            code,
            kind: error.code(),
            message: error.message().into_owned(),
            fields: None,
            detail: None,
        }
    }
}

/// Every machine-readable error code the router itself can emit (independent of
/// any action's error type). Included in the generated TypeScript `ErrorCode`
/// union so clients can exhaustively match.
pub(crate) const BUILTIN_ERROR_CODES: &[&str] = &[
    "error",
    "bad_request",
    "not_found",
    "validation",
    "internal",
];

/// Exposes the set of machine codes an error type can produce, for TypeScript
/// generation. Derived automatically by `#[derive(ResponseError)]`.
///
/// Separate from [`ResponseError`] because the router's own [`Error`] type
/// implements this (with no statically-known codes) but deliberately is not a
/// `ResponseError` — keeping the `From<E: ResponseError>` blanket free of a
/// reflexive conflict.
pub trait ErrorCodes {
    /// All codes a value of this type can produce. Default: none.
    fn error_codes() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &[]
    }
}

impl ErrorCodes for Error {}

/// Extracts a readable message from a caught panic payload.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
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
