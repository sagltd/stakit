use super::decode;
use super::sse_test::{collect, decode_events, frame, text_and_tool};
use crate::provider::test_util::assert_uncertified;
use crate::{ProviderError, StopReason, StreamEvent};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::json;
use std::fmt::Write as _;

#[tokio::test]
async fn complete_stream_handles_crlf_multiline_comments_ping_and_future_events() {
    let mut wire = ": keepalive\r\nretry: 10\r\n\r\n".to_owned();
    for event in text_and_tool() {
        wire.push_str("event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n");
        wire.push_str(
            "event: future_event\r\ndata: {\"type\":\"future_event\",\"new\":true}\r\n\r\n",
        );
        write!(wire, "event: {}\r\n", event["type"].as_str().expect("type")).expect("write String");
        for line in serde_json::to_string_pretty(&event)
            .expect("pretty JSON")
            .lines()
        {
            write!(wire, "data: {line}\r\n").expect("write String");
        }
        wire.push_str("\r\n");
    }
    let events = collect(
        wire.as_bytes()
            .chunks(3)
            .map(Bytes::copy_from_slice)
            .collect(),
    )
    .await;
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::End { .. })))
            .count(),
        1
    );
    assert!(
        matches!(events.last(), Some(Ok(StreamEvent::End { stop:StopReason::ToolUse, usage })) if usage.input_tokens == 3 && usage.output_tokens == 5)
    );
}

#[tokio::test]
async fn every_early_eof_fails_even_after_a_whole_tool_call() {
    let frames = text_and_tool();
    for count in 0..frames.len() {
        let wire: String = frames.iter().take(count).map(frame).collect();
        let events = collect(vec![Bytes::from(wire)]).await;
        assert!(
            matches!(events.last(), Some(Err(ProviderError::Transport(_)))),
            "prefix {count}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
        );
    }
    let wire: String = frames.iter().map(frame).collect();
    let partial = wire
        .get(..wire.len().saturating_sub(1))
        .expect("drop final frame separator");
    let events = collect(vec![Bytes::copy_from_slice(partial.as_bytes())]).await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Transport(_)))
    ));
}

#[tokio::test]
async fn message_stop_rejects_unfinished_tools_and_missing_stop_reason() {
    for mut events in [
        text_and_tool().into_iter().take(6).collect::<Vec<_>>(),
        text_and_tool().into_iter().take(1).collect(),
    ] {
        if events.len() > 1 {
            events.push(json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":5}}));
        }
        events.push(json!({"type":"message_stop"}));
        let decoded = decode_events(&events).await;
        assert_uncertified(&decoded);
    }
}

#[tokio::test]
async fn malformed_known_fields_and_block_sequences_fail_closed() {
    let prefix: String = text_and_tool().iter().take(2).map(frame).collect();
    let malformed = [
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":7}}),
        json!({"type":"content_block_delta", "delta":{"type":"text_delta", "text":"secret"}}),
        json!({"type":"content_block_delta", "index":9, "delta":{"type":"text_delta", "text":"secret"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{}"}}),
        json!({"type":"content_block_stop", "index":9}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        json!({"type":"message_delta", "delta":{"stop_reason":7}, "usage":{"output_tokens":5}}),
        json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":-1}}),
        json!({"type":"message_start", "message":{}}),
        json!({"type":"error", "error":{"type":"overloaded_error"}}),
        json!({"unexpected":"secret"}),
    ];
    for bad in malformed {
        let wire = format!("{prefix}data: {bad}\n\n");
        let events = collect(vec![Bytes::from(wire)]).await;
        let error = events
            .last()
            .expect("error event")
            .as_ref()
            .expect_err("malformed frame rejected");
        assert!(
            matches!(error, ProviderError::Decode { .. }),
            "{bad}: {error:?}"
        );
        assert!(!error.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn invalid_utf8_and_malformed_json_never_surface_raw_payloads() {
    for wire in [
        b"data: {\"type\":\"ping\",\"secret\":\"\xff\"}\n\n".to_vec(),
        b"data: {secret-invalid-json}\n\n".to_vec(),
    ] {
        let events = collect(vec![Bytes::from(wire)]).await;
        let error = events
            .last()
            .expect("error")
            .as_ref()
            .expect_err("invalid bytes rejected");
        assert!(matches!(error, ProviderError::Decode { body, .. } if body.is_empty()));
        assert!(!error.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn midstream_error_and_network_failure_never_emit_success() {
    let mut wire: String = text_and_tool().iter().take(3).map(frame).collect();
    wire.push_str(&frame(&json!({"type":"error", "error":{"type":"rate_limit_error", "message":"wait"}, "request_echo":"secret"})));
    let events = collect(vec![Bytes::from(wire)]).await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Api { status: 429, .. }))
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::End { .. })))
    );
    assert!(
        !events
            .last()
            .expect("error")
            .as_ref()
            .expect_err("API error")
            .to_string()
            .contains("secret")
    );
    let input = futures::stream::iter([Err::<Bytes, _>(ProviderError::Transport(
        "network interrupted".into(),
    ))]);
    let events: Vec<_> = decode(input).collect().await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Transport(_)))
    ));
}
