use super::sse_test::{collect, decode_events, frame, text_and_tool};
use crate::provider::test_util::assert_uncertified;
use crate::{AssistantContent, ProviderError, StreamEvent, Thinking};
use bytes::Bytes;
use serde_json::{Value, json};

pub(super) fn signed_blocks() -> Vec<Value> {
    vec![
        json!({"type":"message_start", "message":{"usage":{"input_tokens":3,"output_tokens":0}}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"thinking", "thinking":"", "signature":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"thinking_delta", "thinking":"first 東京"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"sig-"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"one"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"text", "text":"café "}}),
        json!({"type":"content_block_delta", "index":1, "delta":{"type":"text_delta", "text":"東京"}}),
        json!({"type":"content_block_stop", "index":1}),
        json!({"type":"content_block_start", "index":2, "content_block":{"type":"redacted_thinking", "data":"opaque=blob"}}),
        json!({"type":"content_block_stop", "index":2}),
        json!({"type":"content_block_start", "index":3, "content_block":{"type":"thinking", "thinking":"", "signature":""}}),
        json!({"type":"content_block_delta", "index":3, "delta":{"type":"signature_delta", "signature":"sig-two"}}),
        json!({"type":"content_block_stop", "index":3}),
        json!({"type":"content_block_start", "index":4, "content_block":{"type":"tool_use", "id":"t1", "name":"echo", "input":{}}}),
        json!({"type":"content_block_delta", "index":4, "delta":{"type":"input_json_delta", "partial_json":"{\"city\":\"東京\"}"}}),
        json!({"type":"content_block_stop", "index":4}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":17}}),
        json!({"type":"message_stop"}),
    ]
}

fn blocks(events: &[Result<StreamEvent, ProviderError>]) -> Vec<(u64, AssistantContent)> {
    events
        .iter()
        .filter_map(|event| match event {
            Ok(StreamEvent::AssistantBlock { index, content }) => Some((*index, content.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn completed_blocks_preserve_order_empty_signed_thinking_and_redacted_bytes() {
    let wire: String = signed_blocks().iter().map(frame).collect();
    let events = collect(
        wire.as_bytes()
            .chunks(2)
            .map(Bytes::copy_from_slice)
            .collect(),
    )
    .await;
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        blocks(&events),
        vec![
            (
                0,
                AssistantContent::Thinking(Thinking::Visible {
                    text: "first 東京".into(),
                    signature: Some("sig-one".into())
                })
            ),
            (1, AssistantContent::Text("café 東京".into())),
            (
                2,
                AssistantContent::Thinking(Thinking::Redacted {
                    data: "opaque=blob".into()
                })
            ),
            (
                3,
                AssistantContent::Thinking(Thinking::Visible {
                    text: "".into(),
                    signature: Some("sig-two".into())
                })
            ),
            (
                4,
                AssistantContent::ToolUse {
                    id: "t1".into(),
                    name: "echo".into(),
                    input: json!({"city":"東京"})
                }
            ),
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::SignatureDelta(_))))
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::ToolCall { .. })))
            .count(),
        1
    );
}

#[tokio::test]
async fn indexed_complete_blocks_keep_interleaved_signatures_separate() {
    let mut events = vec![
        json!({"type":"message_start", "message":{"usage":{"input_tokens":3,"output_tokens":0}}}),
    ];
    for index in 0..2 {
        events.push(json!({"type":"content_block_start", "index":index, "content_block":{"type":"thinking", "thinking":""}}));
    }
    for (index, signature) in [(0, "A"), (1, "C"), (0, "B"), (1, "D")] {
        events.push(json!({"type":"content_block_delta", "index":index, "delta":{"type":"signature_delta", "signature":signature}}));
    }
    for index in [1, 0] {
        events.push(json!({"type":"content_block_stop", "index":index}));
    }
    events.push(json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}, "usage":{"output_tokens":4}}));
    events.push(json!({"type":"message_stop"}));
    let decoded = decode_events(&events).await;
    assert_eq!(
        blocks(&decoded),
        vec![
            (
                1,
                AssistantContent::Thinking(Thinking::Visible {
                    text: "".into(),
                    signature: Some("CD".into())
                })
            ),
            (
                0,
                AssistantContent::Thinking(Thinking::Visible {
                    text: "".into(),
                    signature: Some("AB".into())
                })
            ),
        ]
    );
}

#[tokio::test]
async fn malformed_tool_json_fails_without_fabricating_a_complete_tool_block() {
    let mut events = text_and_tool();
    let arguments = events.get_mut(5).expect("tool delta");
    *arguments = json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"{malformed"}});
    let decoded = decode_events(&events).await;
    assert_uncertified(&decoded);
}

/// One tool block started with `input`, optionally followed by `deltas`.
fn single_tool_turn(name: &str, input: &Value, deltas: &[&str]) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"message_start", "message":{"usage":{"input_tokens":3,"output_tokens":0}}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"t1", "name":name, "input":input}}),
    ];
    for partial in deltas {
        events.push(json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":partial}}));
    }
    events.extend([
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]);
    events
}

#[tokio::test]
async fn tool_start_input_is_preserved_when_no_argument_deltas_are_sent() {
    let events = single_tool_turn("echo", &json!({"city":"Paris"}), &[]);
    let decoded = decode_events(&events).await;
    assert_eq!(
        blocks(&decoded),
        vec![(
            0,
            AssistantContent::ToolUse {
                id: "t1".into(),
                name: "echo".into(),
                input: json!({"city":"Paris"})
            }
        )]
    );
}

#[tokio::test]
async fn empty_tool_argument_fragments_preserve_the_initial_empty_object() {
    let events = single_tool_turn("ping", &json!({}), &[""]);
    let decoded = decode_events(&events).await;
    assert!(decoded.iter().all(Result::is_ok), "{decoded:?}");
    assert!(decoded.iter().any(|event| matches!(event,
        Ok(StreamEvent::ToolCall { arguments, .. }) if arguments == "{}"
    )));
    assert!(
        matches!(blocks(&decoded).as_slice(), [(_, AssistantContent::ToolUse { input, .. })] if input == &json!({}))
    );
}
