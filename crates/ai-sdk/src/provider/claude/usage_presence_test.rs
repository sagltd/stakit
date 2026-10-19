//! Completion requires observed counters, while interrupted progress stays useful.
use super::{parse_response, sse::decode};
use crate::{ProviderError, StreamEvent, Usage};
use futures::StreamExt as _;
use serde_json::{Value, json};
use std::fmt::Write as _;

#[test]
fn complete_requires_both_observed_usage_counters() {
    for usage in [
        Value::Null,
        json!({}),
        json!({"input_tokens":0}),
        json!({"output_tokens":0}),
        json!({"input_tokens":null,"output_tokens":0}),
    ] {
        let value = json!({"content":[],"stop_reason":"end_turn","usage":usage});
        assert!(parse_response(&value).is_err());
    }
    assert!(parse_response(&json!({"content":[],"stop_reason":"end_turn"})).is_err());
    let response = parse_response(&json!({"content":[],"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0}})).expect("explicit zero");
    assert_eq!(response.usage, Usage::zero());
}

#[test]
fn complete_rejects_invalid_counters_and_subsets_without_echoing_payload() {
    for usage in [
        json!({"input_tokens":-1,"output_tokens":0}),
        json!({"input_tokens":0,"output_tokens":1,"output_tokens_details":{"thinking_tokens":2}}),
        json!({"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":1,"cache_creation":{"ephemeral_1h_input_tokens":2}}),
        json!({"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":1}}),
    ] {
        let error = parse_response(
            &json!({"content":[],"stop_reason":"end_turn","usage":usage,"secret":"private"}),
        )
        .expect_err("invalid usage");
        assert!(matches!(error, ProviderError::Decode { body, .. } if body.is_empty()));
    }
}

async fn stream(
    start: Value,
    final_usage: Option<Value>,
) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut delta = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}});
    if let Some(usage) = final_usage {
        delta["usage"] = usage;
    }
    let mut wire = String::new();
    for event in [
        json!({"type":"message_start","message":{"usage":start}}),
        delta,
        json!({"type":"message_stop"}),
    ] {
        write!(wire, "data: {event}\n\n").expect("write fixture");
    }
    decode(futures::stream::iter([Ok::<_, String>(
        bytes::Bytes::from(wire),
    )]))
    .collect()
    .await
}

#[tokio::test]
async fn stream_rejects_absent_null_and_partial_usage() {
    for start in [
        Value::Null,
        json!({}),
        json!({"input_tokens":null,"output_tokens":0}),
        json!({"output_tokens":0}),
    ] {
        let events = stream(start, Some(json!({"output_tokens":0}))).await;
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::Decode { .. }))
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
        );
    }
    for final_usage in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"output_tokens":null})),
        Some(json!({"input_tokens":7})),
    ] {
        let events = stream(json!({"input_tokens":7,"output_tokens":0}), final_usage).await;
        assert!(events.iter().any(
            |event| matches!(event,Ok(StreamEvent::Start { usage }) if usage.input_tokens == 7)
        ));
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::Decode { .. }))
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
        );
    }
}

#[tokio::test]
async fn stream_accepts_explicit_zero_only_after_final_delta() {
    let events = stream(
        json!({"input_tokens":0,"output_tokens":0}),
        Some(json!({"output_tokens":0})),
    )
    .await;
    assert!(
        matches!(events.last(),Some(Ok(StreamEvent::End { usage, .. })) if *usage == Usage::zero())
    );
}

#[test]
fn accumulator_requires_final_delta_before_terminal_success() {
    let mut accum = super::Accum::default();
    accum.push(&json!({"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}})).expect("start");
    // A stop reason alone cannot convert the initial output estimate into final usage.
    accum.stop = Some(crate::StopReason::EndTurn);
    assert!(accum.push(&json!({"type":"message_stop"})).is_err());
}

#[test]
fn nullable_optional_cache_fields_keep_documented_zero_default() {
    let response = parse_response(&json!({"content":[],"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":null,"cache_read_input_tokens":null,"cache_creation":null,"output_tokens_details":null}})).expect("nullable optional cache counters");
    assert_eq!(response.usage, Usage::zero());
}

#[tokio::test]
async fn stream_rejects_inconsistent_reasoning_and_cache_subsets() {
    for usage in [
        json!({"output_tokens":1,"output_tokens_details":{"thinking_tokens":2}}),
        json!({"output_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":1}}),
    ] {
        let events = stream(json!({"input_tokens":1,"output_tokens":0}), Some(usage)).await;
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::Decode { .. }))
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
        );
    }
}
