//! What a provider's model accepts, and the one place a request is fitted to it.
//!
//! A provider *declares* its model's [`ProviderCapabilities`]; shared logic
//! (the agent loop, [`LLM`](crate::LLM), and the built-in providers themselves)
//! calls [`ChatRequest::fit_to`] before sending. So a model that rejects forced
//! tool choice, sampling parameters, thinking budgets or a large `max_tokens`
//! never receives them, and a new backend (`OpenAI`, Grok, your own) only states
//! what it takes instead of re-implementing the workarounds.

use crate::provider::{ChatRequest, ThinkingConfig, ToolChoice};

/// Budget sent when a budget-only model is asked for adaptive thinking.
pub(crate) const DEFAULT_THINKING_BUDGET: u32 = 2048;

/// How a model's extended thinking is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingSupport {
    /// No thinking control: requests are fitted to [`ThinkingConfig::Off`].
    #[default]
    None,
    /// The model sizes its own thinking; [`ThinkingConfig::Budget`] is fitted
    /// to [`ThinkingConfig::Adaptive`].
    Adaptive,
    /// Explicit token budgets; [`ThinkingConfig::Adaptive`] is fitted to a
    /// default budget.
    Budget,
}

/// What a provider's model accepts (see [`Provider::capabilities`](crate::Provider::capabilities)).
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent model capability flags, not mutually exclusive states"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Honours [`ToolChoice::Any`], [`ToolChoice::Required`] and
    /// [`ToolChoice::Tool`]; without it they are fitted to [`ToolChoice::Auto`]
    /// (callers that need the call steer from the prompt and check it happened).
    pub forced_tool_choice: bool,
    /// How thinking is configured.
    pub thinking: ThinkingSupport,
    /// Accepts sampling parameters (`temperature`); without it they are dropped.
    pub sampling_params: bool,
    /// Caches prompt prefixes; hits surface in
    /// [`Usage::cache_read_tokens`](crate::Usage::cache_read_tokens).
    pub prompt_cache: bool,
    /// Accepts image inputs. Never fitted (dropping a user's image would lose
    /// content): check it before attaching images.
    pub vision: bool,
    /// The largest `max_tokens` the model accepts; requests are clamped to it.
    pub max_output_tokens: Option<u32>,
}

impl Default for ProviderCapabilities {
    /// Takes the unified request as it is — what every provider received before
    /// capabilities existed — except thinking, which is off because an
    /// undeclared provider may not understand it. Suits test and mock providers.
    fn default() -> Self {
        Self {
            forced_tool_choice: true,
            thinking: ThinkingSupport::None,
            sampling_params: true,
            prompt_cache: false,
            vision: false,
            max_output_tokens: None,
        }
    }
}

/// Whether `choice` forces a tool call.
const fn is_forced(choice: &ToolChoice) -> bool {
    matches!(
        choice,
        ToolChoice::Any | ToolChoice::Required | ToolChoice::Tool(_)
    )
}

/// `requested` mapped onto what the model supports.
const fn fit_thinking(requested: ThinkingConfig, support: ThinkingSupport) -> ThinkingConfig {
    match (requested, support) {
        (_, ThinkingSupport::None) => ThinkingConfig::Off,
        (ThinkingConfig::Budget(_), ThinkingSupport::Adaptive) => ThinkingConfig::Adaptive,
        (ThinkingConfig::Adaptive, ThinkingSupport::Budget) => {
            ThinkingConfig::Budget(DEFAULT_THINKING_BUDGET)
        }
        (requested, _) => requested,
    }
}

impl ChatRequest {
    /// Fits this request to what `capabilities` accepts: forced tool choice
    /// becomes `Auto`, unsupported sampling is dropped, thinking maps onto the
    /// model's style, and `max_tokens` is clamped to the output limit.
    /// Idempotent, so every layer may call it.
    pub fn fit_to(&mut self, capabilities: &ProviderCapabilities) {
        if !capabilities.forced_tool_choice && is_forced(&self.tool_choice) {
            self.tool_choice = ToolChoice::Auto;
        }
        if !capabilities.sampling_params {
            self.temperature = None;
        }
        self.thinking = fit_thinking(self.thinking, capabilities.thinking);
        if let Some(limit) = capabilities.max_output_tokens {
            self.max_tokens = self.max_tokens.min(limit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that asks for everything a model might reject.
    fn demanding() -> ChatRequest {
        let mut request = ChatRequest::new("m");
        request.tool_choice = ToolChoice::Tool("lookup".into());
        request.temperature = Some(0.3);
        request.thinking = ThinkingConfig::Budget(5000);
        request.max_tokens = 1_000_000;
        request
    }

    #[test]
    fn default_capabilities_pass_the_request_through_except_thinking() {
        let mut request = demanding();
        request.fit_to(&ProviderCapabilities::default());
        assert_eq!(request.tool_choice, ToolChoice::Tool("lookup".into()));
        assert_eq!(request.temperature, Some(0.3));
        assert_eq!(request.max_tokens, 1_000_000);
        assert_eq!(request.thinking, ThinkingConfig::Off);
    }

    #[test]
    fn forced_tool_choice_becomes_auto_when_unsupported() {
        let capabilities = ProviderCapabilities {
            forced_tool_choice: false,
            ..ProviderCapabilities::default()
        };
        for forced in [
            ToolChoice::Any,
            ToolChoice::Required,
            ToolChoice::Tool("x".into()),
        ] {
            let mut request = ChatRequest::new("m");
            request.tool_choice = forced;
            request.fit_to(&capabilities);
            assert_eq!(request.tool_choice, ToolChoice::Auto);
        }
        let mut request = ChatRequest::new("m");
        request.tool_choice = ToolChoice::None;
        request.fit_to(&capabilities);
        assert_eq!(request.tool_choice, ToolChoice::None, "None never forces");
    }

    #[test]
    fn temperature_is_dropped_without_sampling_support() {
        let mut request = demanding();
        request.fit_to(&ProviderCapabilities {
            sampling_params: false,
            ..ProviderCapabilities::default()
        });
        assert_eq!(request.temperature, None);
    }

    #[test]
    fn thinking_maps_onto_the_supported_style() {
        let cases = [
            (
                ThinkingConfig::Budget(5000),
                ThinkingSupport::Adaptive,
                ThinkingConfig::Adaptive,
            ),
            (
                ThinkingConfig::Adaptive,
                ThinkingSupport::Budget,
                ThinkingConfig::Budget(DEFAULT_THINKING_BUDGET),
            ),
            (
                ThinkingConfig::Budget(5000),
                ThinkingSupport::Budget,
                ThinkingConfig::Budget(5000),
            ),
            (
                ThinkingConfig::Adaptive,
                ThinkingSupport::Adaptive,
                ThinkingConfig::Adaptive,
            ),
            (
                ThinkingConfig::Adaptive,
                ThinkingSupport::None,
                ThinkingConfig::Off,
            ),
            (
                ThinkingConfig::Off,
                ThinkingSupport::Adaptive,
                ThinkingConfig::Off,
            ),
        ];
        for (requested, support, expected) in cases {
            assert_eq!(
                fit_thinking(requested, support),
                expected,
                "{requested:?} on {support:?}"
            );
        }
    }

    #[test]
    fn max_tokens_is_clamped_to_the_output_limit() {
        let mut request = demanding();
        request.fit_to(&ProviderCapabilities {
            max_output_tokens: Some(128_000),
            ..ProviderCapabilities::default()
        });
        assert_eq!(request.max_tokens, 128_000);
    }

    #[test]
    fn fitting_twice_changes_nothing_more() {
        let capabilities = ProviderCapabilities {
            forced_tool_choice: false,
            thinking: ThinkingSupport::Budget,
            sampling_params: false,
            prompt_cache: true,
            vision: true,
            max_output_tokens: Some(64_000),
        };
        let mut once = demanding();
        once.fit_to(&capabilities);
        let mut twice = once.clone();
        twice.fit_to(&capabilities);
        assert_eq!(once, twice);
    }
}
