use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use stakit_ai_sdk::{
    AssistantContent, CacheCapabilities, CacheTtl, ChatRequest, ChatResponse, EventStream,
    ExplicitCacheCapabilities, Extraction, ExtractionError, LLM, Provider, ProviderCapabilities,
    ProviderError, StopReason, Usage,
};

pub(crate) const SECRET: &str = "PRIVATE-PAYLOAD-DO-NOT-LOG";

#[derive(serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
pub(crate) struct Person {
    #[validate(min_len = 3)]
    pub(crate) name: String,
}

#[derive(Clone)]
pub(crate) struct Recording {
    pub(crate) response: ChatResponse,
    pub(crate) failure: Option<fn() -> ProviderError>,
    pub(crate) requests: Arc<Mutex<Vec<ChatRequest>>>,
    model: String,
}

pub(crate) fn tool(input: Value) -> AssistantContent {
    AssistantContent::ToolUse {
        id: "call-1".into(),
        name: "extract".into(),
        input,
    }
}

impl Recording {
    pub(crate) fn new() -> Self {
        Self {
            response: ChatResponse {
                content: vec![tool(json!({"name":"Dana"}))],
                stop: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 17,
                    output_tokens: 9,
                    cache_read_tokens: 7,
                    cache_create_tokens: 5,
                    cache_create_1h_tokens: 3,
                    reasoning_tokens: 2,
                },
            },
            failure: None,
            requests: Arc::default(),
            model: "flexible".into(),
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Extracts a [`Person`] from the user message "Dana".
    pub(crate) async fn extract_dana(&self) -> Extraction<Person> {
        LLM::new(self.clone())
            .user("Dana")
            .extract_with_usage::<Person>()
            .await
    }

    pub(crate) fn last_request(&self) -> ChatRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

/// Asserts a failed provider call left unknown usage, a payload-free error and
/// no retry.
pub(crate) fn assert_provider_failed(outcome: &Extraction<Person>, provider: &Recording) {
    assert_eq!(outcome.usage, None);
    assert_eq!(
        outcome.result.as_ref().err().copied(),
        Some(ExtractionError::ProviderFailed)
    );
    assert!(!format!("{outcome:?}").contains(SECRET));
    assert_eq!(provider.calls(), 1);
}

impl Provider for Recording {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities_for(&self, model: &str) -> ProviderCapabilities {
        if model == "restricted" {
            ProviderCapabilities {
                forced_tool_choice: false,
                sampling_params: false,
                max_output_tokens: Some(64),
                ..ProviderCapabilities::default()
            }
        } else {
            ProviderCapabilities::default()
        }
    }

    fn cache_capabilities_for(&self, model: &str) -> CacheCapabilities {
        CacheCapabilities::Known {
            implicit: false,
            explicit: Some(ExplicitCacheCapabilities {
                max_points: 4,
                ttls: if model == "restricted" {
                    vec![CacheTtl::FiveMin]
                } else {
                    vec![CacheTtl::FiveMin, CacheTtl::OneHour]
                },
            }),
        }
    }

    fn complete(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        self.requests.lock().unwrap().push(request);
        let result = self
            .failure
            .map_or_else(|| Ok(self.response.clone()), |failure| Err(failure()));
        Box::pin(async move { result })
    }

    fn stream(&self, _: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async { Err(ProviderError::InvalidArgument("unused stream".into())) })
    }
}
