use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use stakit_ai_sdk::{
    AssistantContent, CacheCapabilities, CacheTtl, ChatRequest, ChatResponse, EventStream,
    ExplicitCacheCapabilities, Provider, ProviderCapabilities, ProviderError, StopReason, Usage,
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

    pub(crate) fn last_request(&self) -> ChatRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
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
