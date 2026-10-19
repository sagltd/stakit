//! Malformed known fields fail through the real non-streaming transport.
#![cfg(feature = "openai")]
#[path = "provider_contract/http.rs"]
#[allow(
    dead_code,
    reason = "completion-only probes share HTTP fixtures with SSE probes"
)]
mod fixture;
use serde_json::json;
use stakit_ai_sdk::{
    ChatRequest, OpenAiClient, ProviderError, test_support::contract::assert_completion_error,
};

#[tokio::test]
async fn malformed_known_usage_and_non_function_tool_kinds_are_rejected() {
    let valid = json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]});
    let mut cases = Vec::new();
    for usage in [
        json!(2),
        json!({"prompt_tokens":"100","completion_tokens":-1}),
        json!({"prompt_tokens":10,"prompt_tokens_details":[]}),
        json!({"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":"2"}}),
    ] {
        let mut body = valid.clone();
        body["usage"] = usage;
        cases.push(body);
    }
    cases.push(json!({"choices":[{"message":{"tool_calls":[{"id":"call-1","type":"other","function":{"name":"effect","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}));
    for body in cases {
        let fixture = fixture::http(200, "application/json", body.to_string().into_bytes());
        let provider = OpenAiClient::new("fixture-not-a-key")
            .with_base_url(&fixture.base_url)
            .model("gpt-6-sol");
        assert_completion_error(&provider, ChatRequest::new("gpt-6-sol"), |error| {
            matches!(error, ProviderError::Decode { .. })
        })
        .await
        .expect("known malformed fields fail");
        fixture.finish();
    }
}
