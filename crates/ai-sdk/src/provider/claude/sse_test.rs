use super::decode;
use crate::{ProviderError, StreamEvent};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

pub(super) fn frame(event: &Value) -> String {
    format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().expect("event type")
    )
}

pub(super) fn text_and_tool() -> Vec<Value> {
    vec![
        json!({"type":"message_start", "message":{"usage":{"input_tokens":3,"output_tokens":0}}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"café 東京🙂"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use", "id":"t1", "name":"echo", "input":{}}}),
        json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"{\"city\":\"東京🙂\"}"}}),
        json!({"type":"content_block_stop", "index":1}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]
}

pub(super) async fn collect(chunks: Vec<Bytes>) -> Vec<Result<StreamEvent, ProviderError>> {
    decode(futures::stream::iter(
        chunks.into_iter().map(Ok::<_, ProviderError>),
    ))
    .collect()
    .await
}

/// Decodes `events`, each framed with its `event:` name, as one chunk.
pub(super) async fn decode_events(events: &[Value]) -> Vec<Result<StreamEvent, ProviderError>> {
    collect(vec![Bytes::from(
        events.iter().map(frame).collect::<String>(),
    )])
    .await
}

#[tokio::test]
async fn split_utf8_keeps_text_and_tool_arguments_exact() {
    let wire: String = text_and_tool().iter().map(frame).collect();
    let chunks = wire
        .bytes()
        .map(|byte| Bytes::copy_from_slice(&[byte]))
        .collect();
    let events = collect(chunks).await;
    assert!(
        events.iter().any(
            |event| matches!(event, Ok(StreamEvent::TextDelta(text)) if text == "café 東京🙂")
        )
    );
    assert!(events.iter().any(|event| matches!(event, Ok(StreamEvent::ToolCall { arguments, .. }) if arguments == "{\"city\":\"東京🙂\"}")));
}

#[tokio::test]
async fn error_event_is_a_provider_failure() {
    let wire = frame(
        &json!({"type":"error", "error":{"type":"overloaded_error", "message":"busy"}, "request_echo":"secret"}),
    );
    let events = collect(vec![Bytes::from(wire)]).await;
    assert!(events.iter().any(|event| matches!(event, Err(ProviderError::Api {status:529, kind, message, ..}) if kind == "overloaded_error" && message == "busy")));
}

#[tokio::test]
async fn eof_before_message_stop_is_a_transport_failure() {
    let wire: String = text_and_tool().iter().take(3).map(frame).collect();
    let events = collect(vec![Bytes::from(wire)]).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::Transport(_))))
    );
}

#[tokio::test]
async fn malformed_data_is_a_decode_failure() {
    let events = collect(vec![Bytes::from_static(
        b"event: message_start\ndata: {broken\n\n",
    )])
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(ProviderError::Decode { .. })))
    );
}
