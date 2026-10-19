//! The public extraction API preserves accounting independently of value validity.
#[path = "llm_usage/compatibility.rs"]
mod compatibility;
#[path = "llm_usage/failures.rs"]
mod failures;
#[path = "llm_usage/fixture.rs"]
mod fixture;

use fixture::{Person, Recording, SECRET};
use stakit_ai_sdk::{
    CacheStrategy, CacheTarget, CacheTtl, ExtractionError, LLM, ToolChoice, Usage,
};

#[tokio::test]
async fn success_preserves_all_usage_before_the_value_is_applied() {
    let provider = Recording::new();
    let outcome = LLM::new(provider.clone())
        .user("Dana")
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.model, "flexible");
    assert_eq!(outcome.usage, Some(provider.response.usage));
    assert_eq!(outcome.result.unwrap().name, "Dana");
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn observed_zero_usage_is_not_unknown() {
    let mut provider = Recording::new();
    provider.response.usage = Usage::zero();
    let outcome = LLM::new(provider)
        .user("Dana")
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.usage, Some(Usage::zero()));
}

#[tokio::test]
async fn outcome_debug_redacts_successful_data_without_requiring_debug_on_t() {
    let mut provider = Recording::new();
    provider.response.content = vec![fixture::tool(serde_json::json!({"name":SECRET}))];
    let outcome = LLM::new(provider)
        .user("private input")
        .extract_with_usage::<Person>()
        .await;
    assert!(outcome.result.is_ok());
    assert!(!format!("{outcome:?}").contains(SECRET));
}

#[tokio::test]
async fn model_override_controls_fitting_and_accounting_identity() {
    let provider = Recording::new();
    let outcome = LLM::new(provider.clone())
        .model("restricted")
        .system("Keep the facts.")
        .user("Dana")
        .temperature(0.25)
        .max_tokens(1000)
        .extract_with_usage::<Person>()
        .await;
    assert!(outcome.result.is_ok());
    assert_eq!(outcome.model, "restricted");
    let request = provider.last_request();
    assert_eq!(request.model, outcome.model);
    assert_eq!(request.tool_choice, ToolChoice::Auto);
    assert_eq!(request.temperature, None);
    assert_eq!(request.max_tokens, 64);
    assert!(
        request
            .system
            .unwrap()
            .text
            .starts_with("Keep the facts.\n\nReturn")
    );
}

#[tokio::test]
async fn unsupported_cache_override_fails_before_transport_with_selected_model() {
    let provider = Recording::new();
    let outcome = LLM::new(provider.clone())
        .model("restricted")
        .user("Dana")
        .cache(CacheStrategy::Breakpoints {
            ttl: CacheTtl::OneHour,
            points: vec![CacheTarget::System],
        })
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.model, "restricted");
    assert_eq!(outcome.usage, None);
    assert_eq!(outcome.result.err(), Some(ExtractionError::InvalidRequest));
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn supported_explicit_cache_is_preserved_by_shared_preflight() {
    let provider = Recording::new();
    let cache = CacheStrategy::Breakpoints {
        ttl: CacheTtl::OneHour,
        points: vec![CacheTarget::System],
    };
    let outcome = LLM::new(provider.clone())
        .user("Dana")
        .cache(cache.clone())
        .extract_with_usage::<Person>()
        .await;
    assert!(outcome.result.is_ok());
    assert_eq!(provider.last_request().cache, cache);
}
