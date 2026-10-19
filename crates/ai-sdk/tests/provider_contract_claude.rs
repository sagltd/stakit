//! Exercise the actual Claude adapter through a deterministic local HTTP transport.
#![cfg(feature = "claude")]
#[path = "provider_contract/http.rs"]
mod fixture;
use fixture::{http, sse};
use serde_json::json;
use stakit_ai_sdk::{
    AssistantContent, ChatRequest, ClaudeClient, Message, ProviderError, StopReason, StreamEvent,
    ToolResultPart, Usage, UserContent,
    test_support::contract::{
        ErrorStage, assert_completion, assert_completion_error, assert_request_fitting,
        assert_stream, assert_stream_error,
    },
};

fn request() -> ChatRequest {
    let mut request = ChatRequest::new("claude-haiku-5-5");
    request.messages = vec![Message::user("hello")];
    request
}
const fn usage() -> Usage {
    Usage {
        input_tokens: 10,
        output_tokens: 4,
        cache_create_tokens: 2,
        cache_read_tokens: 3,
        ..Usage::zero()
    }
}
fn start() -> serde_json::Value {
    json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":2,"cache_read_input_tokens":3}}})
}
fn finish(stop: &str) -> [serde_json::Value; 2] {
    [
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ]
}

#[tokio::test]
async fn fake_claude_completion_preserves_tool_results_and_usage_categories() {
    let fixture = http(200, "application/json", json!({"content":[{"type":"text","text":"Handled"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":2,"cache_read_input_tokens":3}}).to_string().into_bytes());
    let provider = ClaudeClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("claude-haiku-5-5");
    let mut request = request();
    request.messages = vec![
        Message::assistant("Earlier"),
        Message::User(vec![UserContent::ToolResult {
            id: "call-1".into(),
            content: vec![ToolResultPart::Text("Tool failed safely".into())],
            is_error: true,
        }]),
    ];
    let request = assert_request_fitting(&provider, &request).expect("fitting");
    let response = assert_completion(
        &provider,
        request,
        &[AssistantContent::Text("Handled".into())],
        &StopReason::EndTurn,
    )
    .await
    .expect("completion contract");
    assert_eq!(response.usage, usage());
    let sent = fixture.finish();
    assert_eq!(sent["messages"][1]["content"][0]["tool_use_id"], "call-1");
    assert_eq!(sent["messages"][1]["content"][0]["is_error"], true);
}

#[tokio::test]
async fn fake_claude_stream_has_a_real_terminal_and_final_cumulative_usage() {
    let mut events = vec![
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    events.extend(finish("end_turn"));
    let fixture = http(200, "text/event-stream", sse(&events));
    let provider = ClaudeClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("claude-haiku-5-5");
    let events = assert_stream(&provider, request(), &StopReason::EndTurn, &usage())
        .await
        .expect("stream contract");
    assert!(events.contains(&StreamEvent::TextDelta("hello".into())));
    assert_eq!(fixture.finish()["stream"], true);
}

#[tokio::test]
async fn fake_claude_stream_preserves_complete_tool_call_ids_and_argument_fragments() {
    let mut events = vec![
        start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"lookup","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"weather\"}"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    events.extend(finish("tool_use"));
    let fixture = http(200, "text/event-stream", sse(&events));
    let provider = ClaudeClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("claude-haiku-5-5");
    let events = assert_stream(&provider, request(), &StopReason::ToolUse, &usage())
        .await
        .expect("tool stream contract");
    assert!(events.iter().any(|event| matches!(event, StreamEvent::ToolCall { id, name, arguments } if id == "call-1" && name == "lookup" && arguments == "{\"query\":\"weather\"}")));
    fixture.finish();
}

#[tokio::test]
async fn fake_claude_http_and_sse_errors_remain_typed_failures() {
    for streaming in [false, true] {
        let fixture = http(
            429,
            "application/json",
            json!({"error":{"type":"rate_limit_error","message":"fixture limit"}})
                .to_string()
                .into_bytes(),
        );
        let provider = ClaudeClient::new("fixture-not-a-key")
            .with_base_url(&fixture.base_url)
            .model("claude-haiku-5-5");
        if streaming {
            assert_stream_error(&provider, request(), ErrorStage::Acquisition, |error| {
                matches!(error, ProviderError::Api { status: 429, .. })
            })
            .await
            .expect("HTTP stream error");
        } else {
            assert_completion_error(&provider, request(), |error| {
                matches!(error, ProviderError::Api { status: 429, .. })
            })
            .await
            .expect("HTTP completion error");
        }
        fixture.finish();
    }
    let fixture = http(
        200,
        "text/event-stream",
        sse(&[
            start(),
            json!({"type":"error","error":{"type":"overloaded_error","message":"fixture overload"}}),
        ]),
    );
    let provider = ClaudeClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("claude-haiku-5-5");
    assert_stream_error(&provider, request(), ErrorStage::Streaming, |error| {
        matches!(error, ProviderError::Api { status: 529, .. })
    })
    .await
    .expect("SSE error contract");
    fixture.finish();
}
