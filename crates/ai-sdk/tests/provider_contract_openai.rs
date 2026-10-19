//! Deterministic HTTP coverage of the actual `OpenAI` adapter.
#![cfg(feature = "openai")]
#[path = "provider_contract/http.rs"]
mod fixture;
use fixture::{http, sse};
use serde_json::json;
use stakit_ai_sdk::{
    AssistantContent, ChatRequest, Message, OpenAiClient, ProviderError, StopReason, Usage,
    test_support::contract::{
        ErrorStage, assert_completion, assert_completion_error, assert_stream, assert_stream_error,
    },
};

fn request() -> ChatRequest {
    let mut request = ChatRequest::new("gpt-6-sol");
    request.messages = vec![Message::user("hello")];
    request
}
fn chunks() -> Vec<serde_json::Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":"héllo"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}),
    ]
}
fn wire(events: &[serde_json::Value]) -> Vec<u8> {
    let mut body = sse(events);
    body.extend_from_slice(b"data: [DONE]\n\n");
    body
}
#[tokio::test]
async fn openai_stream_contract_preserves_disjoint_usage_and_text() {
    let fixture = http(200, "text/event-stream", wire(&chunks()));
    let provider = OpenAiClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("gpt-6-sol");
    let usage = Usage {
        input_tokens: 7,
        output_tokens: 4,
        cache_read_tokens: 3,
        reasoning_tokens: 2,
        ..Usage::zero()
    };
    assert_stream(&provider, request(), &StopReason::EndTurn, &usage)
        .await
        .expect("stream contract");
    fixture.finish();
}
#[tokio::test]
async fn malformed_or_truncated_streams_never_report_success() {
    let mut cases = vec![
        b"data: not-json\n\ndata: [DONE]\n\n".to_vec(),
        wire(&[]),
        sse(&chunks()),
        wire(&[json!({"choices":[{"delta":{"content":"partial"}}]})]),
        wire(&[json!({"choices":[{"delta":{"content":4},"finish_reason":"stop"}]})]),
        b"data: \xff\n\ndata: [DONE]\n\n".to_vec(),
    ];
    cases.push(wire(&[json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}), json!({"choices":[{"delta":{"content":"late"},"finish_reason":null}]})]));
    for body in cases {
        let fixture = http(200, "text/event-stream", body);
        let provider = OpenAiClient::new("fixture-not-a-key")
            .with_base_url(&fixture.base_url)
            .model("gpt-6-sol");
        assert_stream_error(&provider, request(), ErrorStage::Streaming, |error| {
            matches!(
                error,
                ProviderError::Decode { .. } | ProviderError::Transport(_)
            )
        })
        .await
        .expect("invalid stream fails");
        fixture.finish();
    }
}
#[tokio::test]
async fn openai_sse_error_is_a_typed_failure() {
    let fixture = http(
        200,
        "text/event-stream",
        sse(&[
            json!({"error":{"type":"server_error","message":"fixture failure","code":"server_error"}}),
        ]),
    );
    let provider = OpenAiClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("gpt-6-sol");
    assert_stream_error(&provider, request(), ErrorStage::Streaming, |e| {
        matches!(e, ProviderError::Api { .. })
    })
    .await
    .expect("SSE error");
    fixture.finish();
}
#[tokio::test]
async fn invalid_completion_cannot_fabricate_end_turn_or_tool_arguments() {
    for body in [
        json!({}),
        json!({"choices":[{"message":{"content":"hi"}}]}),
        json!({"choices":[{"message":{"tool_calls":[{"id":"call","function":{"name":"f","arguments":"not-json"}}]},"finish_reason":"tool_calls"}]}),
    ] {
        let fixture = http(200, "application/json", body.to_string().into_bytes());
        let provider = OpenAiClient::new("fixture-not-a-key")
            .with_base_url(&fixture.base_url)
            .model("gpt-6-sol");
        assert_completion_error(&provider, request(), |e| {
            matches!(e, ProviderError::Decode { .. })
        })
        .await
        .expect("invalid response fails");
        fixture.finish();
    }
}
#[tokio::test]
async fn completion_refusal_is_preserved() {
    let fixture=http(200,"application/json",json!({"choices":[{"message":{"content":null,"refusal":"Cannot help"},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":0}}).to_string().into_bytes());
    let provider = OpenAiClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("gpt-6-sol");
    assert_completion(
        &provider,
        request(),
        &[AssistantContent::Text("Cannot help".into())],
        &StopReason::Refusal,
    )
    .await
    .expect("refusal");
    fixture.finish();
}

#[tokio::test]
async fn tool_fragments_preserve_identity_and_results_preserve_call_id() {
    use stakit_ai_sdk::{StreamEvent, ToolResultPart, UserContent};
    let fixture = http(
        200,
        "text/event-stream",
        wire(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"look","arguments":"{\"q\":"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"up","arguments":"1}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0}}),
        ]),
    );
    let provider = OpenAiClient::new("fixture-not-a-key")
        .with_base_url(&fixture.base_url)
        .model("gpt-6-sol");
    let mut request = request();
    request.messages = vec![Message::User(vec![UserContent::ToolResult {
        id: "prior-call".into(),
        content: vec![ToolResultPart::Text("Ready".into())],
        is_error: false,
    }])];
    let events = assert_stream(&provider, request, &StopReason::ToolUse, &Usage::zero())
        .await
        .expect("tool contract");
    assert!(events.contains(&StreamEvent::ToolCall {
        id: "call-1".into(),
        name: "lookup".into(),
        arguments: "{\"q\":1}".into()
    }));
    let body = fixture.finish();
    assert_eq!(body["messages"][0]["role"], "tool");
    assert_eq!(body["messages"][0]["tool_call_id"], "prior-call");
    assert_eq!(body["messages"][0]["content"], "Ready");
}

#[tokio::test]
async fn known_models_reject_incompatible_effective_tool_reasoning_before_http() {
    use stakit_ai_sdk::{Provider, ToolDef};
    for model in ["gpt-6-sol", "gpt-6-luna"] {
        for effort in [json!("high"), serde_json::Value::Null] {
            for streaming in [false, true] {
                let provider = OpenAiClient::new("fixture-not-a-key")
                    .with_base_url("http://127.0.0.1:1")
                    .model(model);
                let mut request = request();
                request.model = model.into();
                request.tools = vec![ToolDef::new(
                    "lookup",
                    "lookup",
                    json!({"type":"object","properties":{}}),
                )]
                .into();
                request
                    .extra
                    .insert("reasoning_effort".into(), effort.clone());
                let result = if streaming {
                    provider.stream(request).await.map(|_| ())
                } else {
                    provider.complete(request).await.map(|_| ())
                };
                assert!(
                    matches!(result, Err(ProviderError::InvalidArgument(_))),
                    "must fail before transport"
                );
            }
        }
    }
}
