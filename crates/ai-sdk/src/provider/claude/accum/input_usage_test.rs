//! Inclusive input cannot disappear when provider cache categories are refined.

use crate::{ProviderError, StreamEvent};
use futures::StreamExt as _;
use serde_json::{Value, json};
use std::fmt::Write as _;

fn initial() -> Value {
    json!({"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0}}})
}

fn delta(usage: &Value) -> Value {
    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage})
}

async fn decoded(usage: Value) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut wire = String::new();
    for event in [initial(), delta(&usage), json!({"type":"message_stop"})] {
        write!(wire, "data: {event}\n\n").expect("write fixture");
    }
    super::super::sse::decode(futures::stream::iter([Ok::<_, String>(
        bytes::Bytes::from(wire),
    )]))
    .collect()
    .await
}

#[tokio::test]
async fn final_delta_cannot_erase_previously_observed_inclusive_input() {
    let usage = json!({"input_tokens":0,"output_tokens":1});
    let events = decoded(usage.clone()).await;
    assert!(
        matches!(events.last(), Some(Err(ProviderError::Decode { body, .. })) if body.is_empty())
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
    );
    assert!(events.iter().any(
        |event| matches!(event, Ok(StreamEvent::Start { usage }) if usage.total_input() == 100)
    ));
    let mut accum = super::Accum::default();
    accum.push(&initial()).expect("start");
    assert!(accum.push(&delta(&usage)).is_err());
    assert_eq!(accum.usage.total_input(), 100);
}

#[tokio::test]
async fn cache_redistribution_preserves_the_observed_inclusive_input() {
    let events = decoded(json!({"input_tokens":40,"output_tokens":1,"cache_read_input_tokens":50,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":2}})).await;
    let Some(Ok(StreamEvent::End { usage, .. })) = events.last() else {
        panic!("valid cache refinement requires a terminal response");
    };
    assert_eq!(usage.total_input(), 100);
    assert_eq!(usage.input_tokens, 40);
    assert_eq!(usage.cache_read_tokens, 50);
    assert_eq!(usage.cache_create_tokens, 10);
    assert_eq!(usage.cache_create_1h_tokens, 2);
}
