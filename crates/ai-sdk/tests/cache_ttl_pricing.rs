//! One-hour cache writes are a subset of writes, priced once at their own rate.
use stakit_ai_sdk::{ModelPrice, Pricing, Usage};

const BASE: ModelPrice = ModelPrice {
    input: 2.0,
    output: 10.0,
    cache_read: 0.1,
    cache_write: 2.5,
    cache_write_1h: Some(4.0),
};

const fn mixed_usage() -> Usage {
    Usage {
        input_tokens: 20,
        output_tokens: 10,
        cache_read_tokens: 100,
        cache_create_tokens: 80,
        cache_create_1h_tokens: 30,
        ..Usage::zero()
    }
}

#[test]
fn mixed_write_lifetimes_are_charged_once_at_their_own_rates() {
    let cost = Pricing::new()
        .with("model", BASE)
        .cost("model", &mixed_usage())
        .unwrap();
    assert!((cost - 0.000_395).abs() < 1e-12, "{cost}");
    assert_eq!(mixed_usage().total_input(), 200);
}

#[test]
fn long_prompt_rates_also_apply_to_one_hour_writes() {
    let long = ModelPrice {
        cache_write_1h: Some(8.0),
        ..BASE
    };
    let pricing = Pricing::new().with_tiered("model", BASE, 199, long);
    let cost = pricing.cost("model", &mixed_usage()).unwrap();
    assert!((cost - 0.000_515).abs() < 1e-12, "{cost}");
}

#[test]
fn missing_hourly_rate_is_unknown_instead_of_the_five_minute_price() {
    let price = ModelPrice {
        cache_write_1h: None,
        ..BASE
    };
    let pricing = Pricing::new().with("model", price);
    assert_eq!(pricing.cost("model", &mixed_usage()), None);
    assert_eq!(pricing.cost("model", &Usage::zero()), Some(0.0));
}

#[test]
fn invalid_hourly_subset_is_unknown_instead_of_double_counted() {
    let usage = Usage {
        cache_create_1h_tokens: 81,
        ..mixed_usage()
    };
    assert_eq!(
        Pricing::new().with("model", BASE).cost("model", &usage),
        None
    );
}

#[test]
fn merge_preserves_hourly_write_usage_with_saturation() {
    let mut usage = mixed_usage();
    usage.merge(&mixed_usage());
    assert_eq!(usage.cache_create_tokens, 160);
    assert_eq!(usage.cache_create_1h_tokens, 60);
    let mut usage = Usage {
        cache_create_1h_tokens: u64::MAX,
        ..Usage::zero()
    };
    usage.merge(&mixed_usage());
    assert_eq!(usage.cache_create_1h_tokens, u64::MAX);
}

#[test]
fn old_serialized_usage_defaults_hourly_writes_to_zero() {
    let usage: Usage = serde_json::from_str(r#"{"input_tokens":1,"output_tokens":2,"cache_create_tokens":3,"cache_read_tokens":4,"reasoning_tokens":0}"#).unwrap();
    assert_eq!(usage.cache_create_1h_tokens, 0);
}
