//! Missing usage must never certify a successful zero-cost request.
use super::{response::parse_response, sse::decode};
use crate::provider::test_util::{
    Events, assert_uncertified, collect, end_usage, one_chunk, sse_wire,
};
use crate::{StreamEvent, Usage};
use serde_json::{Value, json};

fn response(usage: Option<Value>) -> Value {
    let mut value = json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]});
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

#[test]
fn complete_requires_both_observed_usage_counters() {
    for usage in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({"prompt_tokens":0})),
        Some(json!({"completion_tokens":0})),
        Some(json!({"prompt_tokens":null,"completion_tokens":0})),
    ] {
        assert!(parse_response(&response(usage)).is_err());
    }
    assert_eq!(
        parse_response(&response(Some(
            json!({"prompt_tokens":0,"completion_tokens":0})
        )))
        .expect("explicit zero")
        .usage,
        Usage::zero()
    );
}

#[test]
fn complete_rejects_impossible_cache_and_reasoning_subsets() {
    for usage in [
        json!({"prompt_tokens":1,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":1,"cache_write_tokens":1}}),
        json!({"prompt_tokens":0,"completion_tokens":1,"completion_tokens_details":{"reasoning_tokens":2}}),
        json!({"prompt_tokens":u64::MAX,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":u64::MAX,"cache_write_tokens":1}}),
    ] {
        assert!(parse_response(&response(Some(usage))).is_err());
    }
}

async fn stream(usage: &[Value], tools: bool) -> Events {
    let choice = if tools {
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"work","arguments":"{}"}}]},"finish_reason":"tool_calls"}]})
    } else {
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
    };
    let footers: Vec<_> = usage
        .iter()
        .map(|usage| json!({"choices":[],"usage":usage}))
        .collect();
    let wire = sse_wire(std::iter::once(&choice).chain(&footers)) + "data: [DONE]\n\n";
    collect(decode(one_chunk(wire))).await
}

#[tokio::test]
async fn stream_requires_usage_before_end_or_tool_release() {
    for usage in [
        vec![],
        vec![Value::Null],
        vec![json!({})],
        vec![json!({"prompt_tokens":0})],
        vec![json!({"completion_tokens":0})],
        vec![json!({"prompt_tokens":null,"completion_tokens":0})],
    ] {
        let events = stream(&usage, true).await;
        assert_uncertified(&events);
    }
}

#[tokio::test]
async fn stream_accepts_explicit_zero_and_incrementally_observed_counters() {
    for usage in [
        vec![json!({"prompt_tokens":0,"completion_tokens":0})],
        vec![json!({"prompt_tokens":0}), json!({"completion_tokens":0})],
    ] {
        let events = stream(&usage, false).await;
        assert_eq!(end_usage(&events), Some(Usage::zero()));
    }
}

#[tokio::test]
async fn missing_final_counter_keeps_known_progress_without_certifying_completion() {
    let events = stream(&[json!({"prompt_tokens":7})], false).await;
    assert!(
        events.iter().any(
            |event| matches!(event, Ok(StreamEvent::Usage { usage }) if usage.input_tokens == 7)
        )
    );
    assert_uncertified(&events);
}

#[tokio::test]
async fn stream_rejects_inconsistent_cache_and_reasoning_subsets() {
    for usage in [
        json!({"prompt_tokens":1,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":2}}),
        json!({"prompt_tokens":0,"completion_tokens":1,"completion_tokens_details":{"reasoning_tokens":2}}),
    ] {
        let events = stream(&[usage], true).await;
        assert_uncertified(&events);
    }
}

#[path = "usage_finality_test.rs"]
mod usage_finality_test;
