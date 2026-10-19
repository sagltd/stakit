use super::fixture::{self, Person, Recording, SECRET};
use stakit_ai_sdk::{AgentError, ExtractionError, LLM, ProviderError, StopReason};

#[derive(serde::Deserialize, stakit_model::JsonSchema)]
struct LegacyPerson {
    name: String,
}

#[tokio::test]
async fn legacy_extract_still_accepts_types_without_validate() {
    let mut provider = Recording::new();
    provider.response.content = vec![fixture::tool(serde_json::json!({"name":"x"}))];
    let value = LLM::new(provider)
        .user("x")
        .extract::<LegacyPerson>()
        .await
        .unwrap();
    assert_eq!(value.name, "x");
}

#[tokio::test]
async fn legacy_extract_preserves_provider_error_details() {
    let mut provider = Recording::new();
    provider.failure = Some(|| ProviderError::Transport(SECRET.into()));
    let error = LLM::new(provider)
        .user("Dana")
        .extract::<LegacyPerson>()
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, AgentError::Provider(ProviderError::Transport(message)) if message == SECRET)
    );
}

#[tokio::test]
async fn end_turn_with_an_actual_extract_call_is_accepted_and_metered() {
    let mut provider = Recording::new();
    provider.response.stop = StopReason::EndTurn;
    let outcome = LLM::new(provider.clone())
        .user("Dana")
        .extract_with_usage::<Person>()
        .await;
    assert!(outcome.result.is_ok());
    assert_eq!(outcome.usage, Some(provider.response.usage));
}

fn reject(_: &str) -> Result<(), stakit_model::ValidationError> {
    Err(stakit_model::ValidationError::new("rejected", SECRET))
}

#[derive(serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
struct Rejected {
    #[validate(custom = reject)]
    name: String,
}

#[tokio::test]
async fn validation_error_text_is_not_retained_in_the_new_error() {
    let provider = Recording::new();
    let outcome = LLM::new(provider.clone())
        .user("Dana")
        .extract_with_usage::<Rejected>()
        .await;
    let error = outcome.result.as_ref().err().copied().unwrap();
    assert_eq!(error, ExtractionError::ValidationFailed);
    assert_eq!(outcome.usage, Some(provider.response.usage));
    assert!(!format!("{error} {error:?} {outcome:?}").contains(SECRET));
    assert!(std::error::Error::source(&error).is_none());
}

#[tokio::test]
async fn provider_errors_never_invent_usage_or_retain_private_details() {
    let failures: [fn() -> ProviderError; 4] = [
        || ProviderError::Decode {
            err: SECRET.into(),
            body: SECRET.into(),
        },
        || ProviderError::Api {
            status: 429,
            kind: SECRET.into(),
            message: SECRET.into(),
            retry_after: None,
        },
        || ProviderError::InvalidArgument(SECRET.into()),
        || ProviderError::Cancelled,
    ];
    for failure in failures {
        let mut provider = Recording::new();
        provider.failure = Some(failure);
        let outcome = LLM::new(provider.clone())
            .user("Dana")
            .extract_with_usage::<Person>()
            .await;
        assert_eq!(outcome.usage, None);
        assert_eq!(
            outcome.result.as_ref().err().copied(),
            Some(ExtractionError::ProviderFailed)
        );
        assert!(!format!("{outcome:?}").contains(SECRET));
        assert_eq!(provider.calls(), 1);
    }
}

#[tokio::test]
async fn validated_extraction_rejects_multiple_tool_blocks_but_preserves_usage() {
    for name in ["extract", "other"] {
        let mut provider = Recording::new();
        provider
            .response
            .content
            .push(stakit_ai_sdk::AssistantContent::ToolUse {
                id: "call-2".into(),
                name: name.into(),
                input: serde_json::json!({"name":"Other"}),
            });
        let outcome = LLM::new(provider.clone())
            .user("Dana")
            .extract_with_usage::<Person>()
            .await;
        assert_eq!(
            outcome.result.as_ref().err().copied(),
            Some(ExtractionError::InvalidPayload)
        );
        assert_eq!(outcome.usage, Some(provider.response.usage));
        assert_eq!(provider.calls(), 1);
    }
}

#[tokio::test]
async fn legacy_extraction_keeps_first_extract_tool_selection() {
    let mut provider = Recording::new();
    provider.response.content.insert(
        0,
        stakit_ai_sdk::AssistantContent::ToolUse {
            id: "unrelated".into(),
            name: "other".into(),
            input: serde_json::json!({"name":"Other"}),
        },
    );
    provider
        .response
        .content
        .push(fixture::tool(serde_json::json!({"name":"Second"})));
    let value = LLM::new(provider)
        .user("Dana")
        .extract::<LegacyPerson>()
        .await
        .unwrap();
    assert_eq!(value.name, "Dana");
}
