//! The client transport error.

/// A failure talking to a stakit server.
///
/// This is returned only for **real** transport failures (connection refused,
/// TLS, malformed response, …). Application errors returned by an action ride
/// inside [`ActionResult::Error`](crate::ActionResult) and never surface here —
/// the same split the TypeScript client makes between `onError` and `isError`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The underlying HTTP request failed (connect, timeout, TLS, …).
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The request parameters could not be serialized.
    #[error("failed to encode parameters: {0}")]
    Encode(serde_json::Error),
    /// The server response could not be deserialized.
    #[error("failed to decode response: {0}")]
    Decode(serde_json::Error),
    /// The response did not contain the requested action's result.
    #[error("response missing result for action `{0}`")]
    MissingAction(&'static str),
    /// A websocket-level failure.
    #[error("websocket error: {0}")]
    WebSocket(String),
    /// The connection closed before a reply arrived.
    #[error("connection closed")]
    Closed,
}
