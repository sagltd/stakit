//! Token-usage accounting and cost estimation.
//!
//! [`Usage`] is the unified token tally that every provider maps its native
//! usage object into. [`Pricing`] turns a `Usage` into an estimated dollar
//! cost via a per-model price table. The cost is an **estimate** — client-side
//! price tables drift; never bill from it.

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

/// A unified token tally for one request (or accumulated across a loop).
///
/// Field mapping: Anthropic `input_tokens` / `output_tokens` /
/// `cache_creation_input_tokens` / `cache_read_input_tokens` /
/// `output_tokens_details.thinking_tokens`; `OpenAI` `prompt_tokens` /
/// `completion_tokens` / `prompt_tokens_details.cached_tokens` /
/// `completion_tokens_details.reasoning_tokens` (`OpenAI` has no cache-write count).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens (tokens after the last cache breakpoint).
    pub input_tokens: u64,
    /// Generated output tokens (includes reasoning tokens).
    pub output_tokens: u64,
    /// Tokens written to the prompt cache this request (Anthropic only).
    pub cache_create_tokens: u64,
    /// Tokens served from the prompt cache this request.
    pub cache_read_tokens: u64,
    /// Reasoning/thinking tokens (a subset of `output_tokens`).
    pub reasoning_tokens: u64,
}

impl Usage {
    /// A zeroed tally (const-constructible).
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_create_tokens: 0,
            cache_read_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    /// Total input tokens across all tiers (uncached + cache write + cache read).
    ///
    /// Saturating so adversarial provider token counts can never overflow-panic
    /// (debug) or silently wrap (release).
    #[must_use]
    pub const fn total_input(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_create_tokens)
            .saturating_add(self.cache_read_tokens)
    }

    /// Adds another tally into this one, field by field. Saturating so a long
    /// accumulated run can never overflow-panic in debug builds.
    pub const fn merge(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_create_tokens = self
            .cache_create_tokens
            .saturating_add(other.cache_create_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

/// Per-model prices, in US dollars per **one million** tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// Price per 1M uncached input tokens.
    pub input: f64,
    /// Price per 1M output tokens.
    pub output: f64,
    /// Price per 1M cache-read (cached input) tokens.
    pub cache_read: f64,
    /// Price per 1M cache-write tokens (Anthropic).
    pub cache_write: f64,
}

/// A model-keyed price table used to estimate request cost.
#[derive(Debug, Clone, Default)]
pub struct Pricing(HashMap<String, ModelPrice>);

impl Pricing {
    /// An empty price table.
    #[must_use]
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Inserts or replaces the price for `model` (builder style).
    #[must_use]
    pub fn with(mut self, model: impl Into<String>, price: ModelPrice) -> Self {
        self.0.insert(model.into(), price);
        self
    }

    /// Looks up the price for `model`, if known.
    #[must_use]
    pub fn price(&self, model: &str) -> Option<ModelPrice> {
        self.0.get(model).copied()
    }

    /// Estimates the dollar cost of `usage` for `model`, or `None` if the model
    /// is not in the table. Reasoning tokens are already counted in
    /// `output_tokens`, so they are not charged twice.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts are far below 2^53; cost is a documented estimate"
    )]
    pub fn cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        let p = self.0.get(model)?;
        let per_m = |tokens: u64, rate: f64| (tokens as f64) / 1_000_000.0 * rate;
        Some(
            per_m(usage.input_tokens, p.input)
                + per_m(usage.output_tokens, p.output)
                + per_m(usage.cache_read_tokens, p.cache_read)
                + per_m(usage.cache_create_tokens, p.cache_write),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sums_each_field() {
        let mut a = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            ..Usage::default()
        };
        a.merge(&Usage {
            input_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 1,
            ..Usage::default()
        });
        assert_eq!(a.input_tokens, 13);
        assert_eq!(a.output_tokens, 9);
        assert_eq!(a.cache_read_tokens, 2);
        assert_eq!(a.reasoning_tokens, 1);
    }

    #[test]
    fn merge_saturates_instead_of_overflowing() {
        // A near-max tally plus more must clamp at u64::MAX, never panic.
        let mut a = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX - 1,
            ..Usage::default()
        };
        a.merge(&Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        });
        assert_eq!(a.input_tokens, u64::MAX);
        assert_eq!(a.output_tokens, u64::MAX);
    }

    #[test]
    fn total_input_sums_all_tiers() {
        let u = Usage {
            input_tokens: 100,
            cache_create_tokens: 20,
            cache_read_tokens: 30,
            ..Usage::default()
        };
        assert_eq!(u.total_input(), 150);
    }

    #[test]
    fn total_input_saturates_instead_of_overflowing() {
        // Adversarial provider token counts near u64::MAX must clamp, not panic.
        let u = Usage {
            input_tokens: u64::MAX,
            cache_create_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            ..Usage::default()
        };
        assert_eq!(u.total_input(), u64::MAX);
    }

    #[test]
    fn cost_uses_per_million_rates() {
        let pricing = Pricing::new().with(
            "m",
            ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_read: 0.30,
                cache_write: 3.75,
            },
        );
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            cache_create_tokens: 1_000_000,
            reasoning_tokens: 500_000,
        };
        // 3 + 15 + 0.30 + 3.75 = 22.05 (reasoning not charged separately).
        let cost = pricing.cost("m", &usage).expect("known model");
        assert!((cost - 22.05).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn cost_is_none_for_unknown_model() {
        assert!(Pricing::new().cost("nope", &Usage::default()).is_none());
    }
}
