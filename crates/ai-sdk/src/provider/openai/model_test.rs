//! Registered model fitting applies to both the handle and request overrides.
use super::*;
use crate::{ProviderCapabilities, ThinkingConfig};

#[test]
fn registered_chat_models_disable_reasoning_for_tool_calls() {
    for model in ["gpt-6-sol", "gpt-6-luna"] {
        let mut request = ChatRequest::new(model);
        request.tools = vec![ToolDef::new(
            "lookup",
            "Read a value",
            json!({"type":"object"}),
        )]
        .into();
        request.temperature = Some(0.4);
        request.thinking = ThinkingConfig::Adaptive;
        request.max_tokens = 200_000;
        let body = build_request_body(&request);
        assert_eq!(body["reasoning_effort"], "none");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["max_completion_tokens"], 128_000);
    }
}

#[test]
fn model_override_selects_its_own_capabilities() {
    let provider = OpenAiClient::new("fake-offline-key").model("gpt-4o");
    assert!(provider.capabilities().sampling_params);
    let actual = provider.capabilities_for("gpt-6-luna");
    assert!(!actual.sampling_params);
    assert!(actual.prompt_cache);
    assert!(actual.vision);
    assert_eq!(actual.max_output_tokens, Some(128_000));
    assert_eq!(
        provider.capabilities_for("custom-model"),
        ProviderCapabilities::default()
    );
}
