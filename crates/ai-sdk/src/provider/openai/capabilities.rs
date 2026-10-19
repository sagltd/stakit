//! Capabilities implemented by this Chat Completions adapter.
use crate::{CacheCapabilities, ProviderCapabilities};

pub(super) fn is_registered_chat_model(model: &str) -> bool {
    matches!(model, "gpt-6-sol" | "gpt-6-luna")
}

pub(super) fn model_capabilities(model: &str) -> ProviderCapabilities {
    if !is_registered_chat_model(model) {
        return ProviderCapabilities::default();
    }
    ProviderCapabilities {
        sampling_params: false,
        prompt_cache: true,
        vision: true,
        max_output_tokens: Some(128_000),
        ..ProviderCapabilities::default()
    }
}

pub(super) fn cache_capabilities(model: &str) -> CacheCapabilities {
    if is_registered_chat_model(model) || matches!(model, "gpt-4o" | "gpt-4o-mini") {
        // New OpenAI APIs support explicit markers, but this adapter currently
        // implements implicit caching only. Reject explicit requests honestly.
        CacheCapabilities::Known {
            implicit: true,
            explicit: None,
        }
    } else {
        CacheCapabilities::Unknown
    }
}
