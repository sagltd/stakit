//! The typed per-action result.

use serde::Deserialize;

use stakit_router::ErrorBody;

/// The outcome of one action call.
///
/// Either success with typed `data`, or an application error. Mirrors the
/// TypeScript client's `isOk` / `isError` split — a network failure is a
/// [`TransportError`](crate::TransportError) instead, never this.
#[derive(Debug, Clone)]
pub enum ActionResult<T> {
    /// The action succeeded.
    Ok(T),
    /// The action returned an application error (`code`, `message`, `fields`).
    Error(ErrorBody),
}

impl<T> ActionResult<T> {
    /// `true` if this is [`ActionResult::Ok`].
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// `true` if this is [`ActionResult::Error`].
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    /// The success data, if any.
    #[must_use]
    pub const fn data(&self) -> Option<&T> {
        match self {
            Self::Ok(data) => Some(data),
            Self::Error(_) => None,
        }
    }

    /// The error body, if any.
    #[must_use]
    pub const fn error(&self) -> Option<&ErrorBody> {
        match self {
            Self::Error(error) => Some(error),
            Self::Ok(_) => None,
        }
    }

    /// Consumes the result into a `Result`, so `?` works on the application error.
    ///
    /// # Errors
    /// Returns the [`ErrorBody`] when this is [`ActionResult::Error`].
    pub fn into_ok(self) -> Result<T, ErrorBody> {
        match self {
            Self::Ok(data) => Ok(data),
            Self::Error(error) => Err(error),
        }
    }

    /// The success data, consuming the result.
    #[must_use]
    pub fn ok(self) -> Option<T> {
        match self {
            Self::Ok(data) => Some(data),
            Self::Error(_) => None,
        }
    }
}

/// Wire shape of one envelope; converted into [`ActionResult`].
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum Envelope<T> {
    Ok { data: T },
    Error { error: ErrorBody },
}

impl<T> From<Envelope<T>> for ActionResult<T> {
    fn from(env: Envelope<T>) -> Self {
        match env {
            Envelope::Ok { data } => Self::Ok(data),
            Envelope::Error { error } => Self::Error(error),
        }
    }
}
