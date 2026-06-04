//! Error types.
//!
//! [`ProviderError`] is what a [`Provider`](crate::Provider) returns; it is a
//! real [`std::error::Error`]. [`ToolError`] is what tool bodies return —
//! deliberately **not** an `Error` so a blanket `From` lets any `?`-propagated
//! error become a tool error ergonomically (the same trick `stakit-router` uses
//! for its `Error`). [`AgentError`] is the crate's top-level error.
//!
//! Surfacing note: [`ProviderError::Api`] carries only the provider's extracted
//! `error.message`, never the raw HTTP body — the body can echo back request
//! material (including the API key) and flows, via `Display`, into the run
//! outcome / SSE. The raw body stays in [`ProviderError::Decode`] only because a
//! decode failure means there is no structured error to extract and the body is
//! needed to debug provider drift.

use std::time::Duration;

use thiserror::Error;

/// An error returned by a [`Provider`](crate::Provider).
///
/// `Debug` is hand-written so [`ProviderError::Decode`]'s raw `body` (which may
/// echo back request fragments) is truncated to its first
/// [`DECODE_BODY_DEBUG_LIMIT`] bytes plus its full length, rather than printed
/// verbatim into logs / panics. `Display` (via `thiserror`) is unaffected.
#[derive(Error)]
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
        /// Concise, safe message extracted from the provider's `error.message`
        /// field (never the raw HTTP body).
        message: String,
        /// Server-requested delay before retrying, parsed from the `Retry-After`
        /// header (seconds). Honored by the retry loop for rate limits.
        retry_after: Option<Duration>,
    },
    /// The request was malformed before sending.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The request was cancelled (e.g. via a cancel token).
    #[error("request cancelled")]
    Cancelled,
}

/// Max bytes of a [`ProviderError::Decode`] body shown in `Debug` output. The
/// raw body can echo request fragments, so it is truncated here.
const DECODE_BODY_DEBUG_LIMIT: usize = 64;

// Hand-written so the raw decode `body` (which may echo request material) is
// truncated in `{:?}` output rather than printed verbatim into logs/panics.
impl std::fmt::Debug for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => f.debug_tuple("Transport").field(msg).finish(),
            Self::Decode { err, body } => {
                // Truncate on a char boundary so a multi-byte body never panics.
                let cut = body
                    .char_indices()
                    .nth(DECODE_BODY_DEBUG_LIMIT)
                    .map_or(body.len(), |(i, _)| i);
                f.debug_struct("Decode")
                    .field("err", err)
                    .field("body_len", &body.len())
                    .field("body_prefix", &&body[..cut])
                    .finish()
            }
            Self::Api {
                status,
                kind,
                message,
                retry_after,
            } => f
                .debug_struct("Api")
                .field("status", status)
                .field("kind", kind)
                .field("message", message)
                .field("retry_after", retry_after)
                .finish(),
            Self::InvalidArgument(msg) => f.debug_tuple("InvalidArgument").field(msg).finish(),
            Self::Cancelled => f.write_str("Cancelled"),
        }
    }
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
pub enum AgentError {
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
    /// A host context (db / store) failure.
    #[error("context error: {0}")]
    Context(String),
}

impl AgentError {
    /// Wrap a host context (db/store) failure.
    pub fn context(e: impl std::fmt::Display) -> Self {
        Self::Context(e.to_string())
    }
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
    fn provider_error_flows_into_agent_error() {
        let ai: AgentError = ProviderError::Cancelled.into();
        assert!(matches!(ai, AgentError::Provider(ProviderError::Cancelled)));
    }

    #[test]
    fn decode_debug_truncates_raw_body() {
        // A long body that echoes request material must not appear verbatim in
        // Debug output; only its length and a short prefix are shown.
        let body = format!("SECRET-LEAK-{}", "x".repeat(500));
        let err = ProviderError::Decode {
            err: "expected value".into(),
            body: body.clone(),
        };
        let dbg = format!("{err:?}");
        assert!(
            !dbg.contains(&body),
            "raw body leaked verbatim into Debug: {dbg}"
        );
        assert!(dbg.contains("body_len"), "Debug must report the body length");
        assert!(
            dbg.len() < body.len(),
            "Debug must be shorter than the raw body"
        );
    }

    #[test]
    fn decode_debug_truncates_on_char_boundary_for_multibyte_body() {
        // A multi-byte body must not panic when truncated mid-codepoint.
        let body = "é".repeat(200);
        let err = ProviderError::Decode {
            err: "bad".into(),
            body,
        };
        // Must not panic.
        let _ = format!("{err:?}");
    }
}
