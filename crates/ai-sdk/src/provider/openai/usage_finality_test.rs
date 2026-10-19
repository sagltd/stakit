//! Final output counters must describe all generated content.
use super::super::sse::decode;
use crate::StreamEvent;
use crate::provider::test_util::{Events, assert_uncertified, collect, one_chunk, sse_wire};
use serde_json::{Value, json};

async fn decoded(events: &[Value]) -> Events {
    collect(decode(one_chunk(sse_wire(events) + "data: [DONE]\n\n"))).await
}

#[tokio::test]
async fn early_output_progress_cannot_certify_later_generated_content() {
    let prefix = [
        json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":7,"completion_tokens":1}}),
        json!({"choices":[{"delta":{"content":"more output"},"finish_reason":"stop"}]}),
    ];
    for footer in [
        None,
        Some(json!({"prompt_tokens":7})),
        Some(json!({"completion_tokens":null})),
    ] {
        let mut wire_events = prefix.to_vec();
        if let Some(usage) = footer {
            wire_events.push(json!({"choices":[],"usage":usage}));
        }
        let events = decoded(&wire_events).await;
        assert!(events.iter().any(|event| matches!(event,Ok(StreamEvent::Usage { usage }) if usage.input_tokens == 7 && usage.output_tokens == 1)));
        assert_uncertified(&events);
    }
}

#[tokio::test]
async fn final_output_can_share_finishing_choice_or_follow_in_partial_footer() {
    for final_on_choice in [false, true] {
        let mut finish = json!({"choices":[{"delta":{"content":"B"},"finish_reason":"stop"}]});
        if final_on_choice {
            finish["usage"] = json!({"completion_tokens":2});
        }
        let mut wire_events = vec![
            json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":7,"completion_tokens":1}}),
            finish,
        ];
        if !final_on_choice {
            wire_events.push(json!({"choices":[],"usage":{"completion_tokens":2}}));
        }
        let events = decoded(&wire_events).await;
        assert!(
            matches!(events.last(),Some(Ok(StreamEvent::End { usage, .. })) if usage.input_tokens == 7 && usage.output_tokens == 2)
        );
    }
}

#[tokio::test]
async fn partial_final_output_preserves_prior_input_and_cache_counters() {
    let events = decoded(&[
        json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":10,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}}),
        json!({"choices":[{"delta":{"content":"B"},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":null,"completion_tokens":2,"prompt_tokens_details":null,"completion_tokens_details":null}}),
    ]).await;
    assert!(
        matches!(events.last(),Some(Ok(StreamEvent::End { usage, .. })) if usage.input_tokens == 5 && usage.cache_read_tokens == 3 && usage.cache_create_tokens == 2 && usage.output_tokens == 2 && usage.reasoning_tokens == 1)
    );
}

#[tokio::test]
async fn decreasing_final_output_cannot_erase_known_progress() {
    let events = decoded(&[
        json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":7,"completion_tokens":2}}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"completion_tokens":1}}),
    ]).await;
    assert!(
        events.iter().any(
            |event| matches!(event,Ok(StreamEvent::Usage { usage }) if usage.output_tokens == 2)
        )
    );
    assert!(
        !events.iter().any(
            |event| matches!(event,Ok(StreamEvent::Usage { usage }) if usage.output_tokens < 2)
        )
    );
    assert_uncertified(&events);
}

#[tokio::test]
async fn decreasing_inclusive_prompt_usage_cannot_erase_known_input() {
    let events = decoded(&[
        json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":100,"completion_tokens":1}}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":0,"completion_tokens":1}}),
    ]).await;
    assert!(events.iter().any(
        |event| matches!(event,Ok(StreamEvent::Usage { usage }) if usage.total_input() == 100)
    ));
    assert_uncertified(&events);
}

#[tokio::test]
async fn inclusive_prompt_guard_allows_cache_category_refinement() {
    let events = decoded(&[
        json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":10,"completion_tokens":1}}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}}),
    ]).await;
    assert!(
        matches!(events.last(),Some(Ok(StreamEvent::End { usage, .. })) if usage.input_tokens == 5 && usage.cache_read_tokens == 3 && usage.cache_create_tokens == 2 && usage.total_input() == 10)
    );
}

#[path = "usage_totals_test.rs"]
mod usage_totals_test;
