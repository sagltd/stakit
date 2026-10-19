//! The actual adapter must preserve malformed arguments for agent validation.
#![cfg(feature = "openai")]
#[path = "provider_contract/http.rs"]
mod fixture;
use serde_json::json;
use stakit_ai_sdk::{
    ChatRequest, OpenAiClient, StopReason, StreamEvent, Usage,
    test_support::contract::assert_stream,
};

#[tokio::test]
async fn empty_whitespace_and_omitted_tool_arguments_are_never_fabricated_as_objects() {
    for arguments in [None, Some(""), Some(" \t")] {
        let mut function = json!({"name":"effect"});
        if let Some(arguments) = arguments {
            function["arguments"] = json!(arguments);
        }
        let mut body = fixture::sse(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":function}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0}}),
        ]);
        body.extend_from_slice(b"data: [DONE]\n\n");
        let fixture = fixture::http(200, "text/event-stream", body);
        let provider = OpenAiClient::new("fixture-not-a-key")
            .with_base_url(&fixture.base_url)
            .model("gpt-6-sol");
        let events = assert_stream(
            &provider,
            ChatRequest::new("gpt-6-sol"),
            &StopReason::ToolUse,
            &Usage::zero(),
        )
        .await
        .expect("complete raw call");
        assert!(events.iter().any(|event|matches!(event,StreamEvent::ToolCall{arguments:actual,..} if actual==arguments.unwrap_or_default())));
        fixture.finish();
    }
}
