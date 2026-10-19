//! Prompt-cache controls shared by provider adapters.
//!
//! The SDK does not build a token cache. Adapters declare the implicit caching
//! and explicit breakpoint controls they actually implement using
//! [`CacheCapabilities`]. Eligibility, retention and cache hits remain under
//! the provider's control. In particular, [`CacheStrategy::Off`] suppresses
//! explicit markers; it does not promise to disable provider-managed caching.
//! Hits surface uniformly via [`Usage::cache_read_tokens`](crate::Usage).

use serde::{Deserialize, Serialize};

mod capabilities;
pub use capabilities::{CacheCapabilities, ExplicitCacheCapabilities};

/// How prompt caching is applied to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CacheStrategy {
    /// No explicit caching, including author-supplied cache markers.
    /// Provider-managed implicit caching may still occur.
    Off,
    /// Use the adapter's best available caching behavior (the default).
    ///
    /// An adapter may place explicit breakpoints, use implicit prefix caching,
    /// or have no cache support. This is not a cache-hit or retention guarantee.
    #[default]
    Auto,
    /// Place explicit breakpoints at the given targets.
    ///
    /// Requires declared explicit support for this TTL; unsupported controls
    /// return an error before transport. Targets beyond the declared maximum
    /// may be capped in the adapter's documented priority order.
    Breakpoints {
        /// Cache lifetime for the written entries.
        ttl: CacheTtl,
        /// Where to place breakpoints (capped at the provider maximum).
        points: Vec<CacheTarget>,
    },
}

/// Prompt-cache entry lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CacheTtl {
    /// Five-minute explicit cache, only where the adapter declares support.
    #[default]
    FiveMin,
    /// Thirty-minute explicit cache, only where the adapter declares support.
    ThirtyMin,
    /// One-hour explicit cache, only where the adapter declares support.
    OneHour,
}

/// A location at which to place a cache breakpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheTarget {
    /// After the tool definitions.
    Tools,
    /// After the system prompt.
    System,
    /// After the most recent user message.
    LastUserMessage,
    /// After the message at this index in the history.
    MessageIndex(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_strategy_is_auto() {
        assert_eq!(CacheStrategy::default(), CacheStrategy::Auto);
    }

    #[test]
    fn breakpoints_round_trip_through_json() {
        let s = CacheStrategy::Breakpoints {
            ttl: CacheTtl::OneHour,
            points: vec![CacheTarget::Tools, CacheTarget::MessageIndex(3)],
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: CacheStrategy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }
}
