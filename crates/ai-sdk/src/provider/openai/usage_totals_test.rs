//! Optional totals must agree with the counters they summarize.
use super::decoded;
use crate::provider::openai::response::parse_response;
use crate::{ProviderError, StreamEvent};
use serde_json::json;

#[test]
fn absent_total_is_valid_but_present_total_must_match_without_overflow() {
    for usage in [
        json!({"prompt_tokens":0,"completion_tokens":0}),
        json!({"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}),
    ] {
        assert!(parse_response(&json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":usage})).is_ok());
    }
    for usage in [
        json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":100}),
        json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":null}),
        json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":-1}),
        json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":"0"}),
        json!({"prompt_tokens":u64::MAX,"completion_tokens":1,"total_tokens":u64::MAX}),
    ] {
        assert!(parse_response(&json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":usage})).is_err());
    }
}

#[tokio::test]
async fn stale_partial_total_does_not_reject_fresh_counters_before_final_footer() {
    let events = decoded(&[
        json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}),
        json!({"choices":[],"usage":{"completion_tokens":2}}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"completion_tokens":3,"total_tokens":10}}),
    ]).await;
    assert!(
        matches!(events.last(),Some(Ok(StreamEvent::End { usage, .. })) if usage.total_input() == 7 && usage.output_tokens == 3)
    );
}

#[tokio::test]
async fn partial_total_is_checked_when_both_counters_become_observed() {
    for total in [8, 9] {
        let events = decoded(&[
            json!({"choices":[{"delta":{"content":"A"},"finish_reason":null}],"usage":{"total_tokens":total}}),
            json!({"choices":[],"usage":{"prompt_tokens":7}}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"completion_tokens":1}}),
        ]).await;
        if total == 8 {
            assert!(matches!(events.last(), Some(Ok(StreamEvent::End { .. }))));
        } else {
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
}
