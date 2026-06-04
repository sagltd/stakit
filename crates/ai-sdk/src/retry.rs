//! Retry + per-attempt timeout policy for provider calls.
use std::time::Duration;

use crate::error::ProviderError;

/// Retry + timeout policy applied to each provider request.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Max retry attempts after the first try.
    pub max_retries: u32,
    /// Per-attempt timeout (a hung connection trips this).
    pub timeout: Duration,
    /// Base backoff for the first retry.
    pub base_backoff: Duration,
    /// Backoff ceiling.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            timeout: Duration::from_secs(60),
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Coarse retry classification of an attempt failure.
#[derive(Debug, Clone)]
pub enum Retryable {
    /// Transient (connection reset, timeout, 5xx) — retry.
    Transient,
    /// Rate limited — retry, honoring `retry_after` if present.
    RateLimited {
        /// Server-provided delay before retrying, if any.
        retry_after: Option<Duration>,
    },
    /// Permanent (4xx≠429, malformed request, decode) — do not retry.
    Fatal,
}

impl RetryPolicy {
    /// Whether a classified failure should be retried.
    #[must_use]
    pub const fn is_retryable(&self, r: &Retryable) -> bool {
        !matches!(r, Retryable::Fatal)
    }

    /// Backoff for attempt `n` (0-based): exponential, capped at `max_backoff`.
    #[must_use]
    pub fn backoff(&self, n: u32) -> Duration {
        let factor = 1u32.checked_shl(n.min(16)).unwrap_or(u32::MAX);
        self.base_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// Classify a provider error for retry decisions.
///
/// - [`ProviderError::Transport`] → [`Retryable::Transient`]
/// - [`ProviderError::Api`] with status 429 → [`Retryable::RateLimited`]
/// - [`ProviderError::Api`] with status ≥ 500 → [`Retryable::Transient`]
/// - [`ProviderError::Cancelled`] → [`Retryable::Fatal`] (propagate, never retry)
/// - All other variants (4xx≠429, [`ProviderError::Decode`], [`ProviderError::InvalidArgument`])
///   → [`Retryable::Fatal`]
#[must_use]
pub const fn classify(err: &ProviderError) -> Retryable {
    match err {
        ProviderError::Transport(_) => Retryable::Transient,
        ProviderError::Api { status, .. } if *status == 429 => {
            Retryable::RateLimited { retry_after: None }
        }
        ProviderError::Api { status, .. } if *status >= 500 => Retryable::Transient,
        // Cancelled, 4xx≠429, Decode, InvalidArgument — do not retry
        _ => Retryable::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_classification() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 2);
        assert!(p.is_retryable(&Retryable::Transient));
        assert!(!p.is_retryable(&Retryable::Fatal));
        assert!(p.backoff(1) >= p.base_backoff);
        assert!(p.backoff(100) <= p.max_backoff);
    }

    #[test]
    fn classify_transport_is_transient() {
        let err = ProviderError::Transport("connection reset".into());
        assert!(matches!(classify(&err), Retryable::Transient));
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let err = ProviderError::Api {
            status: 429,
            kind: "rate_limit_error".into(),
            message: "too many requests".into(),
        };
        assert!(matches!(
            classify(&err),
            Retryable::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn classify_5xx_is_transient() {
        let err = ProviderError::Api {
            status: 503,
            kind: "overloaded_error".into(),
            message: "service unavailable".into(),
        };
        assert!(matches!(classify(&err), Retryable::Transient));
    }

    #[test]
    fn classify_4xx_non_429_is_fatal() {
        let err = ProviderError::Api {
            status: 400,
            kind: "invalid_request_error".into(),
            message: "bad request".into(),
        };
        assert!(matches!(classify(&err), Retryable::Fatal));
    }

    #[test]
    fn classify_cancelled_is_fatal() {
        let err = ProviderError::Cancelled;
        assert!(matches!(classify(&err), Retryable::Fatal));
    }

    #[test]
    fn classify_decode_is_fatal() {
        let err = ProviderError::Decode {
            err: "unexpected field".into(),
            body: "{}".into(),
        };
        assert!(matches!(classify(&err), Retryable::Fatal));
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff(0), Duration::from_millis(250));
        assert_eq!(p.backoff(1), Duration::from_millis(500));
        assert_eq!(p.backoff(2), Duration::from_secs(1));
        assert_eq!(p.backoff(100), p.max_backoff);
    }
}
