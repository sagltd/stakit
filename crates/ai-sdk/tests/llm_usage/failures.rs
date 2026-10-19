use super::fixture::{self, Person, Recording, SECRET};
use stakit_ai_sdk::{AssistantContent, ExtractionError, LLM, ProviderError, StopReason};

#[tokio::test]
async fn malformed_typed_payload_keeps_usage_without_echoing_payload() {
    let mut provider = Recording::new();
    provider.response.content = vec![fixture::tool(
        serde_json::json!({"name": {"private":SECRET}}),
    )];
    let outcome = LLM::new(provider.clone())
        .user("Dana")
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.usage, Some(provider.response.usage));
    let error = outcome.result.err().unwrap();
    assert_eq!(error, ExtractionError::InvalidPayload);
    assert!(!format!("{error:?} {error}").contains(SECRET));
    assert!(std::error::Error::source(&error).is_none());
}

#[tokio::test]
async fn validation_failure_keeps_usage_and_never_returns_an_unchecked_value() {
    let mut provider = Recording::new();
    provider.response.content = vec![fixture::tool(serde_json::json!({"name":"x"}))];
    let outcome = LLM::new(provider.clone())
        .user("Dana")
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.usage, Some(provider.response.usage));
    assert_eq!(
        outcome.result.err(),
        Some(ExtractionError::ValidationFailed)
    );
}

#[tokio::test]
async fn missing_extract_tool_keeps_usage_and_does_not_accept_json_prose() {
    for content in [
        vec![AssistantContent::Text("{\"name\":\"Dana\"}".into())],
        vec![],
    ] {
        let mut provider = Recording::new();
        provider.response.content = content;
        let outcome = LLM::new(provider.clone())
            .user("Dana")
            .extract_with_usage::<Person>()
            .await;
        assert_eq!(outcome.usage, Some(provider.response.usage));
        assert_eq!(outcome.result.err(), Some(ExtractionError::MissingTool));
        assert_eq!(provider.calls(), 1);
    }
}

#[tokio::test]
async fn refused_or_incomplete_responses_keep_usage_even_with_valid_tool_payloads() {
    for (stop, expected) in [
        (StopReason::Refusal, ExtractionError::Refused),
        (StopReason::MaxTokens, ExtractionError::Incomplete),
        (StopReason::Pause, ExtractionError::Incomplete),
        (
            StopReason::StopSequence(SECRET.into()),
            ExtractionError::Incomplete,
        ),
        (
            StopReason::Other(SECRET.into()),
            ExtractionError::Incomplete,
        ),
    ] {
        let mut provider = Recording::new();
        provider.response.stop = stop;
        let outcome = LLM::new(provider.clone())
            .user("Dana")
            .extract_with_usage::<Person>()
            .await;
        assert_eq!(outcome.usage, Some(provider.response.usage));
        assert_eq!(outcome.result.err(), Some(expected));
        assert_eq!(provider.calls(), 1);
    }
}

#[tokio::test]
async fn provider_failure_has_unknown_usage_and_no_payload_or_retry() {
    let mut provider = Recording::new();
    provider.failure = Some(|| ProviderError::Transport(SECRET.into()));
    let outcome = LLM::new(provider.clone())
        .model("requested-model")
        .user("private input")
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.model, "requested-model");
    assert_eq!(outcome.usage, None);
    assert_eq!(
        outcome.result.as_ref().err().copied(),
        Some(ExtractionError::ProviderFailed)
    );
    assert!(!format!("{outcome:?}").contains(SECRET));
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn missing_user_is_preflight_failure_without_a_provider_call() {
    let provider = Recording::new();
    let outcome = LLM::new(provider.clone())
        .model("selected-model")
        .extract_with_usage::<Person>()
        .await;
    assert_eq!(outcome.model, "selected-model");
    assert_eq!(outcome.usage, None);
    assert_eq!(outcome.result.err(), Some(ExtractionError::MissingUser));
    assert_eq!(provider.calls(), 0);
}
