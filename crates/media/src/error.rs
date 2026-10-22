//! The crate's error type.

/// What can go wrong; the message is safe to log, never contains secrets.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// Missing or malformed configuration.
    #[error("media config: {0}")]
    Config(String),
    /// The object store refused or could not be reached.
    #[error("object store {operation}: {detail}")]
    Store {
        /// Which call failed.
        operation: &'static str,
        /// Why.
        detail: String,
    },
    /// The bytes are not an image this crate can decode or encode.
    #[error("image: {0}")]
    Image(String),
}

impl MediaError {
    pub(crate) fn store(operation: &'static str, detail: impl std::fmt::Display) -> Self {
        Self::Store {
            operation,
            detail: detail.to_string(),
        }
    }
}
