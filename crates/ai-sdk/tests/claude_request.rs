//! Offline request-body tests for each Claude model family: what the built-in
//! Claude provider sends for sampling, tool choice, thinking and the output cap
//! (no network). Claude 5.x rejects fields older models accepted, so every
//! family gets its own expectations.
#![cfg(feature = "claude")]

use std::sync::Arc;

use serde_json::{Value, json};
use stakit_ai_sdk::{
    ChatRequest, Message, ThinkingConfig, ToolChoice, ToolDef, test_support::claude_body,
};

const HAIKU_5_5: &str = "claude-haiku-5-5";
const SONNET_5_5: &str = "claude-sonnet-5-5";
const OPUS_5_5: &str = "claude-opus-5-5";
const MODELS_5_5: [&str; 3] = [HAIKU_5_5, SONNET_5_5, OPUS_5_5];

/// A one-turn request for `model`.
fn request(model: &str) -> ChatRequest {
    let mut request = ChatRequest::new(model);
    request.messages = vec![Message::user("hi")];
    request
}

/// A request that forces the `lookup` tool.
fn forcing_a_tool(model: &str) -> ChatRequest {
    let mut request = request(model);
    request.tools = Arc::from([ToolDef::new(
        "lookup",
        "Look something up.",
        json!({ "type": "object" }),
    )]);
    request.tool_choice = ToolChoice::Tool("lookup".into());
    request
}

/// The body sent for `model` with `thinking` configured.
fn thinking_body(model: &str, thinking: ThinkingConfig) -> Value {
    let mut request = request(model);
    request.thinking = thinking;
    claude_body(&request)
}

#[test]
fn claude_5_5_models_never_receive_a_temperature() {
    for model in MODELS_5_5 {
        let mut request = request(model);
        request.temperature = Some(0.2);
        let body = claude_body(&request);
        assert!(body.get("temperature").is_none(), "{model}: {body:#}");
    }
}

#[test]
fn models_that_accept_sampling_keep_the_temperature() {
    for model in ["claude-haiku-4-5-20251001", "claude-sonnet-4-6"] {
        let mut request = request(model);
        request.temperature = Some(0.25);
        let body = claude_body(&request);
        assert_eq!(body["temperature"], json!(0.25), "{model}: {body:#}");
    }
}

#[test]
fn opus_and_sonnet_5_5_get_auto_instead_of_a_forced_tool() {
    for model in [SONNET_5_5, OPUS_5_5] {
        let body = claude_body(&forcing_a_tool(model));
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "auto" }),
            "{model}: {body:#}"
        );
    }
}

#[test]
fn haiku_5_5_keeps_a_forced_tool() {
    let body = claude_body(&forcing_a_tool(HAIKU_5_5));
    assert_eq!(
        body["tool_choice"],
        json!({ "type": "tool", "name": "lookup" })
    );
}

#[test]
fn adaptive_thinking_is_sent_as_adaptive_to_5_5_models() {
    for model in MODELS_5_5 {
        let body = thinking_body(model, ThinkingConfig::Adaptive);
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive" }),
            "{model}: {body:#}"
        );
    }
}

#[test]
fn a_thinking_budget_becomes_adaptive_on_adaptive_models() {
    for model in [OPUS_5_5, "claude-opus-4-8"] {
        let body = thinking_body(model, ThinkingConfig::Budget(3000));
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive" }),
            "{model}: {body:#}"
        );
    }
}

#[test]
fn a_thinking_budget_is_sent_to_budget_models() {
    let body = thinking_body("claude-haiku-4-5-20251001", ThinkingConfig::Budget(3000));
    assert_eq!(
        body["thinking"],
        json!({ "type": "enabled", "budget_tokens": 3000 })
    );
}

#[test]
fn adaptive_thinking_becomes_a_budget_on_budget_models() {
    let body = thinking_body("claude-sonnet-4-5", ThinkingConfig::Adaptive);
    assert_eq!(body["thinking"]["type"], "enabled", "{body:#}");
    assert!(body["thinking"]["budget_tokens"].is_u64(), "{body:#}");
}

#[test]
fn thinking_off_disables_thinking_on_haiku_5_5() {
    let body = thinking_body(HAIKU_5_5, ThinkingConfig::Off);
    assert_eq!(body["thinking"], json!({ "type": "disabled" }));
}

#[test]
fn thinking_off_is_between_tools_on_sonnet_5_5() {
    let body = thinking_body(SONNET_5_5, ThinkingConfig::Off);
    assert_eq!(body["thinking"], json!({ "type": "between_tools" }));
}

#[test]
fn thinking_off_omits_the_field_on_opus_5_5_which_always_thinks() {
    let body = thinking_body(OPUS_5_5, ThinkingConfig::Off);
    assert!(body.get("thinking").is_none(), "{body:#}");
}

#[test]
fn thinking_off_omits_the_field_on_models_that_think_only_when_asked() {
    for model in ["claude-opus-4-8", "claude-haiku-4-5-20251001"] {
        let body = thinking_body(model, ThinkingConfig::Off);
        assert!(body.get("thinking").is_none(), "{model}: {body:#}");
    }
}

#[test]
fn thinking_off_keeps_adaptive_thinking_at_xhigh_effort() {
    // `between_tools` / `disabled` are a 400 above `high` effort.
    let mut request = request(SONNET_5_5);
    request
        .extra
        .insert("output_config".into(), json!({ "effort": "xhigh" }));
    let body = claude_body(&request);
    assert!(body.get("thinking").is_none(), "{body:#}");
    assert_eq!(body["output_config"]["effort"], "xhigh");
}

#[test]
fn max_tokens_is_clamped_to_the_model_output_limit() {
    let mut request = request(OPUS_5_5);
    request.max_tokens = 500_000;
    let body = claude_body(&request);
    assert_eq!(body["max_tokens"], 128_000);
}

#[test]
fn platform_prefixed_ids_get_the_same_rules() {
    for model in [
        "anthropic.claude-sonnet-5-5",
        "us.anthropic.claude-opus-5-5-v1:0",
    ] {
        let mut request = forcing_a_tool(model);
        request.temperature = Some(0.5);
        let body = claude_body(&request);
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "auto" }),
            "{model}: {body:#}"
        );
        assert!(body.get("temperature").is_none(), "{model}: {body:#}");
    }
}

#[test]
fn unknown_and_newer_models_get_the_strictest_rules() {
    for model in ["claude-sonnet-7", "my-gateway-alias"] {
        let mut request = forcing_a_tool(model);
        request.temperature = Some(0.5);
        let body = claude_body(&request);
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "auto" }),
            "{model}: {body:#}"
        );
        assert!(body.get("temperature").is_none(), "{model}: {body:#}");
    }
}

#[test]
fn claude_models_declare_their_capabilities() {
    use stakit_ai_sdk::{ClaudeClient, Provider, ThinkingSupport};

    let client = ClaudeClient::new("test-key");
    let haiku = client.model(HAIKU_5_5).capabilities();
    assert!(haiku.forced_tool_choice && !haiku.sampling_params);
    assert_eq!(haiku.thinking, ThinkingSupport::Adaptive);
    assert_eq!(haiku.max_output_tokens, Some(128_000));

    for model in [SONNET_5_5, OPUS_5_5] {
        let capabilities = client.model(model).capabilities();
        assert!(!capabilities.forced_tool_choice, "{model}");
        assert!(!capabilities.sampling_params, "{model}");
    }

    let legacy = client.model("claude-haiku-4-5-20251001").capabilities();
    assert!(legacy.sampling_params && legacy.forced_tool_choice);
    assert_eq!(legacy.thinking, ThinkingSupport::Budget);
    assert!(legacy.prompt_cache && legacy.vision);
}
