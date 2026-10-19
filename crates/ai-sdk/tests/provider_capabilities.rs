//! The shared LLM/agent layers fit requests even for third-party providers.
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::json;
use stakit_ai_sdk::{
    Agent, AgentError, AssistantContent, ChatRequest, ChatResponse, EventStream, LLM, Message,
    Provider, ProviderCapabilities, ProviderError, StopReason, StreamEvent, ToolChoice, Usage,
    event_stream,
};

#[derive(Clone)]
struct RecordingProvider {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    model: String,
    capabilities: ProviderCapabilities,
    returns_tool: bool,
    stop: StopReason,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            requests: Arc::default(),
            model: "third-party".to_owned(),
            capabilities: ProviderCapabilities {
                forced_tool_choice: false,
                sampling_params: false,
                max_output_tokens: Some(64),
                ..ProviderCapabilities::default()
            },
            returns_tool: true,
            stop: StopReason::ToolUse,
        }
    }

    fn last_request(&self) -> ChatRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

impl Provider for RecordingProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities_for(&self.model)
    }

    fn capabilities_for(&self, model: &str) -> ProviderCapabilities {
        if model == "flexible" {
            ProviderCapabilities::default()
        } else {
            self.capabilities
        }
    }

    fn complete(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        self.requests.lock().unwrap().push(request);
        let content = if self.returns_tool {
            vec![AssistantContent::ToolUse {
                id: "call-1".into(),
                name: "extract".into(),
                input: json!({ "name": "Dana" }),
            }]
        } else {
            vec![AssistantContent::Text("plain text".into())]
        };
        let stop = self.stop.clone();
        Box::pin(async move {
            Ok(ChatResponse {
                content,
                stop,
                usage: Usage::zero(),
            })
        })
    }

    fn stream(&self, request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            Ok(event_stream(vec![
                Ok(StreamEvent::TextDelta("hello".to_owned())),
                Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Usage::zero(),
                }),
            ]))
        })
    }
}

#[derive(Debug, serde::Deserialize, stakit_model::JsonSchema)]
struct Person {
    name: String,
}

#[tokio::test]
async fn extraction_fits_an_unsupported_forced_tool_choice_and_preserves_the_user_system() {
    let provider = RecordingProvider::new();
    let person: Person = LLM::new(provider.clone())
        .system("Use only the supplied facts.")
        .user("The name is Dana")
        .extract()
        .await
        .unwrap();
    assert_eq!(person.name, "Dana");
    let request = provider.last_request();
    assert_eq!(request.tool_choice, ToolChoice::Auto);
    let system = &request.system.unwrap().text;
    assert!(system.starts_with("Use only the supplied facts."));
    assert!(
        system.contains("extract"),
        "fallback must explicitly request the tool"
    );
    assert_eq!(request.tools.first().unwrap().name, "extract");
}

#[tokio::test]
async fn extraction_keeps_forced_tool_choice_when_supported() {
    let mut provider = RecordingProvider::new();
    provider.capabilities.forced_tool_choice = true;
    LLM::new(provider.clone())
        .system("Use the facts.")
        .user("Dana")
        .extract::<Person>()
        .await
        .unwrap();
    let request = provider.last_request();
    assert_eq!(request.tool_choice, ToolChoice::Tool("extract".into()));
    assert_eq!(&*request.system.unwrap().text, "Use the facts.");
}

#[tokio::test]
async fn plain_text_completion_fits_sampling_and_output_limit() {
    let provider = RecordingProvider::new();
    LLM::new(provider.clone())
        .user("hi")
        .temperature(0.25)
        .max_tokens(1000)
        .text()
        .await
        .unwrap();
    let request = provider.last_request();
    assert_eq!(request.temperature, None);
    assert_eq!(request.max_tokens, 64);
}

#[tokio::test]
async fn extraction_fails_without_a_tool_result_and_does_not_retry() {
    let mut provider = RecordingProvider::new();
    provider.returns_tool = false;
    let error = LLM::new(provider.clone())
        .user("Dana")
        .extract::<Person>()
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::Schema(_)));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn agent_loop_fits_the_selected_providers_output_limit() {
    let provider = RecordingProvider::new();
    let mut agent = Agent::new(())
        .model("third-party")
        .max_tokens(1000)
        .with_context(vec![Message::user("hi")])
        .provider(provider.clone());
    agent.run().await.unwrap();
    assert_eq!(provider.last_request().max_tokens, 64);
}

#[tokio::test]
async fn model_override_uses_the_requested_models_sampling_and_output_rules() {
    let provider = RecordingProvider::new();
    LLM::new(provider.clone())
        .model("flexible")
        .user("hi")
        .temperature(0.25)
        .max_tokens(1000)
        .text()
        .await
        .unwrap();
    let request = provider.last_request();
    assert_eq!(request.temperature, Some(0.25));
    assert_eq!(request.max_tokens, 1000);
}

#[tokio::test]
async fn model_override_adds_extraction_fallback_for_the_requested_restrictive_model() {
    let mut provider = RecordingProvider::new();
    provider.model = "flexible".to_owned();
    LLM::new(provider.clone())
        .model("restrictive")
        .user("Dana")
        .extract::<Person>()
        .await
        .unwrap();
    let request = provider.last_request();
    assert_eq!(request.tool_choice, ToolChoice::Auto);
    assert!(request.system.unwrap().text.contains("extract"));
}

#[tokio::test]
async fn extraction_never_certifies_a_refused_or_incomplete_tool_result() {
    for stop in [
        StopReason::Refusal,
        StopReason::MaxTokens,
        StopReason::Pause,
    ] {
        let mut provider = RecordingProvider::new();
        provider.stop = stop;
        let error = LLM::new(provider.clone())
            .user("Dana")
            .extract::<Person>()
            .await
            .expect_err("refused/incomplete extraction must fail even with a valid tool object");
        assert!(matches!(error, AgentError::Schema(_)));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }
}
