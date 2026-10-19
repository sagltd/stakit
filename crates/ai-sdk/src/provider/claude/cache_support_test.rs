use super::{ClaudeClient, cache_control};
use crate::{
    CacheCapabilities, CacheStrategy, CacheTarget, CacheTtl, ChatRequest,
    ExplicitCacheCapabilities, Provider, ProviderError,
};

#[test]
fn claude_declares_explicit_breakpoints_and_only_supported_lifetimes() {
    let model = ClaudeClient::new("never-sent").model("claude-sonnet-5-5");
    let expected = CacheCapabilities::Known {
        implicit: false,
        explicit: Some(ExplicitCacheCapabilities {
            max_points: 4,
            ttls: vec![CacheTtl::FiveMin, CacheTtl::OneHour],
        }),
    };
    assert_eq!(model.cache_capabilities(), expected);
    assert_eq!(model.cache_capabilities_for("claude-haiku-5-5"), expected);
}

#[tokio::test]
async fn unsupported_cache_lifetime_fails_before_transport_for_both_entrypoints() {
    let model = ClaudeClient::new("never-sent")
        .with_base_url("invalid URL")
        .model("claude-sonnet-5-5");
    let mut request = ChatRequest::new("claude-sonnet-5-5");
    request.cache = CacheStrategy::Breakpoints {
        ttl: CacheTtl::ThirtyMin,
        points: vec![CacheTarget::System],
    };
    let complete = model.complete(request.clone()).await;
    assert!(matches!(complete, Err(ProviderError::InvalidArgument(_))));
    let stream = model.stream(request).await;
    assert!(matches!(stream, Err(ProviderError::InvalidArgument(_))));
}

#[test]
fn offline_body_helper_never_substitutes_an_unsupported_lifetime() {
    // Actual complete/stream entrypoints reject this TTL before serialization;
    // offline inspection keeps the explicit request rather than inventing 5m.
    assert_eq!(
        cache_control(CacheTtl::ThirtyMin)
            .get("ttl")
            .and_then(serde_json::Value::as_str),
        Some("30m")
    );
}

#[test]
fn nested_one_hour_write_usage_is_preserved_without_double_counting_total_writes() {
    let usage = super::map_usage(
        &serde_json::json!({"input_tokens":3,"output_tokens":2,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_5m_input_tokens":6,"ephemeral_1h_input_tokens":4}}),
    );
    assert_eq!(usage.cache_create_tokens, 10);
    assert_eq!(usage.cache_create_1h_tokens, 4);
}

#[test]
fn streaming_one_hour_write_usage_carries_the_final_lifetime_breakdown() {
    let mut accum = super::Accum::default();
    accum.push(&serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":4}}}})).expect("start");
    accum.push(&serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":7}}})).expect("usage");
    let events = accum
        .push(&serde_json::json!({"type":"message_stop"}))
        .expect("stop");
    assert!(
        matches!(events.last(),Some(crate::StreamEvent::End {usage,..}) if usage.cache_create_tokens == 10 && usage.cache_create_1h_tokens == 7)
    );
}

#[test]
fn omitted_stream_lifetime_breakdown_retains_the_initial_hour_write_usage() {
    let mut accum = super::Accum::default();
    accum.push(&serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":4}}}})).expect("start");
    accum.push(&serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})).expect("usage");
    let events = accum
        .push(&serde_json::json!({"type":"message_stop"}))
        .expect("stop");
    assert!(
        matches!(events.last(),Some(crate::StreamEvent::End {usage,..}) if usage.cache_create_tokens == 10 && usage.cache_create_1h_tokens == 4)
    );
}

#[test]
fn streaming_usage_progress_is_published_before_the_terminal_event() {
    let mut accum = super::Accum::default();
    accum.push(&serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":4}}}})).expect("start");
    let events = accum.push(&serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2,"cache_creation":{"ephemeral_1h_input_tokens":7}}})).expect("usage");
    assert!(
        matches!(events.as_slice(), [crate::StreamEvent::Usage { usage }] if usage.input_tokens == 3 && usage.output_tokens == 2 && usage.cache_create_tokens == 10 && usage.cache_create_1h_tokens == 7)
    );
}
