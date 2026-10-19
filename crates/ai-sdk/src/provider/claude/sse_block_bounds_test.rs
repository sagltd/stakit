use super::super::Accum;
use crate::ProviderError;
use crate::provider::{
    MAX_STREAM_BLOCK_BYTES, MAX_STREAM_RESPONSE_BLOCKS, MAX_STREAM_RESPONSE_BYTES,
};
use serde_json::json;

#[test]
fn thinking_text_and_signature_share_the_same_block_budget() {
    let mut accum = Accum::default();
    accum.push(&json!({"type":"content_block_start", "index":0, "content_block":{"type":"thinking", "thinking":""}})).expect("start");
    accum.push(&json!({"type":"content_block_delta", "index":0, "delta":{"type":"thinking_delta", "thinking":"x".repeat(MAX_STREAM_BLOCK_BYTES - 1)}})).expect("text fits");
    let error = accum.push(&json!({"type":"content_block_delta", "index":0, "delta":{"type":"signature_delta", "signature":"xx"}})).expect_err("combined block overflow");
    assert!(matches!(error, ProviderError::Decode { .. }));
}

#[test]
fn sequential_blocks_share_a_bounded_response_budget() {
    let mut accum = Accum::default();
    for index in 0..MAX_STREAM_RESPONSE_BYTES / MAX_STREAM_BLOCK_BYTES {
        accum.push(&json!({"type":"content_block_start", "index":index, "content_block":{"type":"text", "text":""}})).expect("start");
        accum.push(&json!({"type":"content_block_delta", "index":index, "delta":{"type":"text_delta", "text":"x".repeat(MAX_STREAM_BLOCK_BYTES)}})).expect("response fits");
        accum
            .push(&json!({"type":"content_block_stop", "index":index}))
            .expect("stop");
    }
    let error = accum.push(&json!({"type":"content_block_start", "index":99, "content_block":{"type":"text", "text":"x"}})).expect_err("response overflow");
    assert!(matches!(error, ProviderError::Decode { .. }));
}

#[test]
fn empty_sequential_blocks_cannot_bypass_the_response_block_count_cap() {
    let mut accum = Accum::default();
    for index in 0..MAX_STREAM_RESPONSE_BLOCKS {
        accum.push(&json!({"type":"content_block_start", "index":index, "content_block":{"type":"text", "text":""}})).expect("start");
        accum
            .push(&json!({"type":"content_block_stop", "index":index}))
            .expect("stop");
    }
    let error = accum.push(&json!({"type":"content_block_start", "index":MAX_STREAM_RESPONSE_BLOCKS, "content_block":{"type":"text", "text":""}})).expect_err("block count overflow");
    assert!(matches!(error, ProviderError::Decode { .. }));
}
