//! A terminal tally must follow all content and cannot erase known progress.

use super::Accum;
use crate::{ProviderError, StreamEvent};
use futures::StreamExt as _;
use serde_json::{Value, json};
use std::fmt::Write as _;

fn initial() -> Value {
    json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0}}})
}

fn block(index: u64, kind: &str) -> Value {
    json!({"type":"content_block_start","index":index,"content_block":{"type":kind,"text":"","id":"late-call","name":"work","input":{}}})
}

fn close(index: u64) -> Value {
    json!({"type":"content_block_stop","index":index})
}

fn usage(output: u64, reason: Option<&str>) -> Value {
    json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":output}})
}

async fn decoded(events: &[Value]) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut wire = String::new();
    for event in events {
        write!(wire, "data: {event}\n\n").expect("write fixture");
    }
    super::super::sse::decode(futures::stream::iter([Ok::<_, String>(
        bytes::Bytes::from(wire),
    )]))
    .collect()
    .await
}

fn assert_rejected(events: &[Result<StreamEvent, ProviderError>]) {
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Decode { body, .. })) if body.is_empty()
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
    );
}

#[tokio::test]
async fn decoder_rejects_message_delta_before_every_wire_block_closes() {
    for kind in ["text", "future_block"] {
        let events = decoded(&[
            initial(),
            block(0, kind),
            usage(1, Some("end_turn")),
            close(0),
            json!({"type":"message_stop"}),
        ])
        .await;
        assert_rejected(&events);
    }
}

#[tokio::test]
async fn decoder_rejects_new_text_and_tools_after_terminal_usage() {
    for kind in ["text", "tool_use", "future_block"] {
        let events = decoded(&[
            initial(),
            usage(1, Some("end_turn")),
            block(0, kind),
            close(0),
            json!({"type":"message_stop"}),
        ])
        .await;
        assert_rejected(&events);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::ToolCall { .. })))
        );
    }
}

#[test]
fn direct_accumulator_cannot_bypass_open_wire_block_validation() {
    for kind in ["text", "future_block"] {
        let mut accum = Accum::default();
        accum.push(&initial()).expect("start");
        accum.push(&block(0, kind)).expect("open block");
        assert!(accum.push(&usage(1, Some("end_turn"))).is_err());
    }
}

#[test]
fn direct_accumulator_rejects_every_content_event_after_terminal_delta() {
    for event in [
        block(0, "text"),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"late"}}),
        close(0),
    ] {
        let mut accum = Accum::default();
        accum.push(&initial()).expect("start");
        accum
            .push(&usage(1, Some("end_turn")))
            .expect("terminal usage");
        assert!(accum.push(&event).is_err());
    }
}

#[tokio::test]
async fn intermediate_usage_only_delta_can_precede_later_content() {
    let events = decoded(&[
        initial(),
        usage(1, None),
        block(0, "text"),
        close(0),
        usage(2, Some("end_turn")),
        json!({"type":"message_stop"}),
    ])
    .await;
    assert!(
        matches!(events.last(), Some(Ok(StreamEvent::End { usage, .. })) if usage.output_tokens == 2)
    );
}

#[tokio::test]
async fn decreasing_final_output_is_rejected_without_losing_known_progress() {
    let events = decoded(&[
        initial(),
        usage(2, None),
        usage(1, Some("end_turn")),
        json!({"type":"message_stop"}),
    ])
    .await;
    assert_rejected(&events);
    assert!(events.iter().any(
        |event| matches!(event, Ok(StreamEvent::Usage { usage }) if usage.output_tokens == 2)
    ));
    let mut accum = Accum::default();
    accum.push(&initial()).expect("start");
    accum.push(&usage(2, None)).expect("progress");
    assert!(accum.push(&usage(1, Some("end_turn"))).is_err());
    assert_eq!(accum.usage.output_tokens, 2);
}

#[test]
fn direct_final_delta_cannot_reuse_prior_progress_when_output_is_missing() {
    let mut accum = Accum::default();
    accum.push(&initial()).expect("start");
    accum.push(&usage(2, None)).expect("progress");
    let missing_output =
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}});
    assert!(accum.push(&missing_output).is_err());
    assert_eq!(accum.usage.output_tokens, 2);
    assert!(accum.push(&json!({"type":"message_stop"})).is_err());
}
