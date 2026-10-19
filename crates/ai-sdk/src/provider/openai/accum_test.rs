use super::*;
use crate::provider::{MAX_STREAM_BLOCK_BYTES, MAX_STREAM_RESPONSE_BYTES};
use serde_json::json;

#[test]
fn combined_tool_identity_name_and_arguments_share_one_block_budget() {
    let mut accum = Accum::default();
    accum.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"n".repeat(MAX_STREAM_BLOCK_BYTES/2),"arguments":""}}]}}]})).expect("half block");
    assert!(accum.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"a".repeat(MAX_STREAM_BLOCK_BYTES/2)}}]}}]})).is_err());
}
#[test]
fn retained_tool_buffers_share_the_response_budget() {
    let mut accum = Accum::default();
    let args = "a".repeat(MAX_STREAM_BLOCK_BYTES - 32);
    for index in 0..MAX_STREAM_RESPONSE_BYTES / MAX_STREAM_BLOCK_BYTES {
        accum.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":format!("c{index}"),"function":{"name":"f","arguments":args}}]}}]})).expect("within aggregate budget");
    }
    assert!(accum.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":20,"id":"over","function":{"name":"f","arguments":args}}]}}]})).is_err());
}

#[test]
fn tool_calls_use_dense_original_indices_instead_of_arrival_order() {
    let mut accum = Accum::default();
    for index in [1, 0] {
        accum.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":format!("c{index}"),"function":{"name":"f","arguments":"{}"}}]}}]})).expect("tool fragment");
    }
    accum
        .push(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}))
        .expect("reason");
    accum
        .push(&json!({"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0}}))
        .expect("explicit usage");
    let events = accum.finish().expect("complete dense calls");
    let ids = events
        .iter()
        .filter_map(|event| {
            if let StreamEvent::ToolCall { id, .. } = event {
                Some(id.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, ["c0", "c1"]);
    let mut sparse = Accum::default();
    sparse.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c1","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]})).expect("tool fragment");
    assert!(sparse.finish().is_err());
}
