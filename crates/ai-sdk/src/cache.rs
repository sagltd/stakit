//! Prompt-cache strategy.
//!
//! Both major providers cache server-side: Anthropic via explicit
//! `cache_control` breakpoints, `OpenAI` automatically over a stable prefix. The
//! SDK does not build its own token cache — it keeps a stable, append-only
//! prefix and (for Anthropic) places breakpoints per this strategy. On
//! providers with automatic caching the strategy is a no-op, but cache hits
//! still surface uniformly via [`Usage::cache_read_tokens`](crate::Usage).

use serde::{Deserialize, Serialize};

/// How prompt caching is applied to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CacheStrategy {
    /// No explicit caching.
    Off,
    /// Automatically place cache breakpoints (the default; maximizes cache hits
    /// with zero configuration).
    ///
    /// **Claude:** up to three breakpoints are placed in priority order — after
    /// tool definitions (if any), after the system prompt (if any), and rolling
    /// on the previous-turn boundary — subject to the provider's 4-breakpoint
    /// cap.
    ///
    /// **`OpenAI`:** a stable `prompt_cache_key` is derived from the session and
    /// the provider caches the prefix automatically; no explicit breakpoints are
    /// written.
    #[default]
    Auto,
    /// Place explicit breakpoints at the given targets.
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
    /// Five-minute cache (cheaper writes).
    #[default]
    FiveMin,
    /// One-hour cache (more expensive writes, longer reuse).
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
