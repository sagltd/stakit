//! Error types.
//!
//! [`ProviderError`] is what a [`Provider`](crate::Provider) returns; it is a
//! real [`std::error::Error`] carrying the raw response body for drift
//! debugging. [`ToolError`] is what tool bodies return — deliberately **not**
//! an `Error` so a blanket `From` lets any `?`-propagated error become a tool
//! error ergonomically (the same trick `stakit-router` uses for its `Error`).
//! [`AiError`] is the crate's top-level error.

use thiserror::Error;

/// An error returned by a [`Provider`](crate::Provider).
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Network / transport failure talking to the provider.
    #[error("transport error: {0}")]
    Transport(String),
    /// The response could not be decoded into the expected shape.
    #[error("failed to decode provider response: {err}")]
    Decode {
        /// Decoder error message.
        err: String,
        /// Raw response body, kept for debugging provider drift.
        body: String,
    },
    /// The provider returned an error status.
    #[error("provider error {status} ({kind}): {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Provider error kind/type string.
        kind: String,
        /// Human-readable message.
        message: String,
    },
    /// The request was malformed before sending.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The request was cancelled (e.g. via a cancel token).
    #[error("request cancelled")]
    Cancelled,
}

/// An error returned from a tool body.
///
/// Surfaced back to the model as an `is_error` tool result (the model may
/// retry). Intentionally not a [`std::error::Error`] so the blanket `From`
/// below can accept any error via `?`.
#[derive(Debug, Clone)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    /// Builds a tool error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The error message (sent to the model as the tool result).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

// A blanket `From<String>`/`From<&str>` would conflict with the blanket below
// (the compiler must assume `String` could implement `Error` in future). Build
// message errors with [`ToolError::new`] instead.
//
// Blanket conversion so tool bodies can `?` any standard error. Sound because
// `ToolError` does not implement `std::error::Error` (no reflexive overlap).
impl<E: std::error::Error + Send + Sync + 'static> From<E> for ToolError {
    fn from(err: E) -> Self {
        Self {
            message: err.to_string(),
        }
    }
}

/// The crate's top-level error.
#[derive(Debug, Error)]
pub enum AiError {
    /// A provider call failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// A tool failed in a way that aborts the loop (as opposed to a normal
    /// `is_error` result that the model can retry).
    #[error("tool error: {0}")]
    Tool(ToolError),
    /// Tool arguments failed JSON Schema / validation.
    #[error("invalid tool arguments: {0}")]
    Schema(String),
    /// An MCP transport or protocol error.
    #[error("mcp error: {0}")]
    Mcp(String),
    /// A skill could not be loaded.
    #[error("skill error: {0}")]
    Skill(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_displays_message() {
        let e = ToolError::new("boom");
        assert_eq!(e.to_string(), "boom");
        assert_eq!(e.message(), "boom");
    }

    #[test]
    fn standard_errors_convert_into_tool_error() {
        fn parse() -> Result<i32, ToolError> {
            let n: i32 = "not a number".parse()?; // ParseIntError -> ToolError
            Ok(n)
        }
        let err = parse().unwrap_err();
        assert!(err.message().contains("invalid digit"), "{err}");
    }

    #[test]
    fn provider_error_flows_into_ai_error() {
        let ai: AiError = ProviderError::Cancelled.into();
        assert!(matches!(ai, AiError::Provider(ProviderError::Cancelled)));
    }
}
