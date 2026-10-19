//! Cache buckets must partition the provider's inclusive prompt total.
use super::*;
use crate::{ModelPrice, Pricing};

#[test]
fn cached_input_is_not_charged_again_as_ordinary_input() {
    let usage = map_usage(&json!({
        "prompt_tokens": 100,
        "completion_tokens": 20,
        "prompt_tokens_details": { "cached_tokens": 80 },
        "completion_tokens_details": { "reasoning_tokens": 10 }
    }));
    assert_eq!(usage.input_tokens, 20);
    assert_eq!(usage.cache_read_tokens, 80);
    assert_eq!(usage.total_input(), 100);
    assert_eq!(usage.output_tokens, 20);
    assert_eq!(usage.reasoning_tokens, 10);
    let pricing = Pricing::new().with(
        "model",
        ModelPrice {
            input: 2.0,
            output: 10.0,
            cache_read: 0.1,
            cache_write: 2.5,
            cache_write_1h: None,
        },
    );
    let actual = pricing.cost("model", &usage).unwrap();
    assert!((actual - 0.000_248).abs() < 1e-12);
}

#[test]
fn cache_writes_are_separate_from_reads_and_ordinary_input() {
    let usage = map_usage(&json!({
        "prompt_tokens": 100,
        "prompt_tokens_details": { "cached_tokens": 60, "cache_write_tokens": 30 }
    }));
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.cache_read_tokens, 60);
    assert_eq!(usage.cache_create_tokens, 30);
    assert_eq!(usage.total_input(), 100);
}

#[test]
fn missing_cache_details_preserve_legacy_usage() {
    let usage = map_usage(&json!({ "prompt_tokens": 12, "completion_tokens": 4 }));
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.cache_read_tokens, 0);
    assert_eq!(usage.cache_create_tokens, 0);
    assert_eq!(map_usage(&json!({})), Usage::zero());
}

#[test]
fn inconsistent_cache_buckets_cannot_underflow() {
    let usage = map_usage(&json!({
        "prompt_tokens": 1,
        "prompt_tokens_details": {
            "cached_tokens": u64::MAX, "cache_write_tokens": u64::MAX
        }
    }));
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.total_input(), u64::MAX);
}
