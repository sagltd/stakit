//! The provider abstraction: a vendor-neutral LLM backend.
//!
//! [`Provider`] is intentionally small so a third party can implement a new
//! backend (Gemini, Mistral, a local model) in a single file. Built-in
//! reference implementations live behind the `claude` / `openai` features.
//!
//! Requests and responses use the unified [`crate::message`] model; each
//! provider maps its own wire format in and out, accumulating streamed
//! tool-call argument fragments internally so a [`StreamEvent::ToolCall`] is
//! only emitted once whole.

use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AssistantContent, CacheStrategy, Message, ProviderError, SystemPrompt, Usage};

#[cfg(feature = "claude")]
pub(crate) mod claude;
#[cfg(feature = "openai")]
pub(crate) mod openai;

/// A tool definition as sent to the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Tool name (must be unique within a request).
    pub name: String,
    /// Natural-language description used by the model to decide when to call it.
    pub description: String,
    /// JSON Schema object describing the tool's arguments.
    pub parameters: Value,
    /// Request strict schema adherence where the provider supports it.
    pub strict: bool,
    /// Attach a prompt-cache breakpoint after this tool (Anthropic).
    pub cache: bool,
}

impl ToolDef {
    /// Builds a minimal tool definition (non-strict, uncached).
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            strict: false,
            cache: false,
        }
    }
}

/// Controls whether and how the model may call tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolChoice {
    /// Model decides freely (may call zero or more tools).
    #[default]
    Auto,
    /// Model must call at least one tool.
    Any,
    /// Model must call exactly one tool (provider-enforced).
    Required,
    /// Model may not call tools.
    None,
    /// Force a specific tool by name.
    Tool(String),
}

/// Extended-thinking / reasoning configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThinkingConfig {
    /// No extended thinking.
    #[default]
    Off,
    /// Enable with a provider-chosen budget.
    Adaptive,
    /// Enable with an explicit token budget.
    Budget(u32),
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// Natural end of the assistant turn.
    EndTurn,
    /// Hit the `max_tokens` limit.
    MaxTokens,
    /// Emitted one of the configured stop sequences (the matched string).
    StopSequence(String),
    /// The model produced tool calls and is waiting for results.
    ToolUse,
    /// The model refused on policy grounds.
    Refusal,
    /// A long turn was paused and should be resumed.
    Pause,
    /// An unrecognized future value (kept so new provider values never break us).
    Other(String),
}

/// A request to a provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    /// Model id (provider-specific).
    pub model: String,
    /// Optional system prompt.
    pub system: Option<SystemPrompt>,
    /// Conversation history.
    pub messages: Vec<Message>,
    /// Tools available this turn (the active set; see deferred tools).
    pub tools: Vec<ToolDef>,
    /// Tool-choice policy.
    pub tool_choice: ToolChoice,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature, if overriding the provider default.
    pub temperature: Option<f32>,
    /// Stop sequences.
    pub stop_sequences: Vec<String>,
    /// Extended-thinking configuration.
    pub thinking: ThinkingConfig,
    /// Prompt-cache strategy.
    pub cache: CacheStrategy,
    /// Raw per-provider passthrough merged into the request body.
    pub extra: serde_json::Map<String, Value>,
}

impl ChatRequest {
    /// A request for `model` with sensible defaults (4096 max tokens, auto
    /// tool-choice, auto caching).
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            thinking: ThinkingConfig::Off,
            cache: CacheStrategy::Auto,
            extra: serde_json::Map::new(),
        }
    }
}

/// A non-streamed provider response.
#[derive(Debug, Clone)]
pub struct ChatResponse<R> {
    /// Normalized assistant output blocks.
    pub content: Vec<AssistantContent>,
    /// Why generation stopped.
    pub stop: StopReason,
    /// Token usage for this request.
    pub usage: Usage,
    /// Provider-native response body (escape hatch).
    pub raw: R,
}

/// A normalized streaming event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// Stream opened; carries the initial (input) usage where available.
    Start {
        /// Partial usage known at start (e.g. input tokens).
        usage: Usage,
    },
    /// A chunk of assistant text.
    TextDelta(String),
    /// A chunk of reasoning text.
    ReasoningDelta(String),
    /// A reasoning integrity signature chunk (Anthropic).
    SignatureDelta(String),
    /// A complete tool call (argument fragments are accumulated internally).
    ToolCall {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed arguments.
        input: Value,
    },
    /// Stream finished; carries the stop reason and final cumulative usage.
    End {
        /// Why generation stopped.
        stop: StopReason,
        /// Final usage for the request.
        usage: Usage,
    },
}

/// A boxed stream of streaming events.
pub type EventStream = futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>;

/// A vendor-neutral LLM backend.
///
/// Implementors map [`ChatRequest`] to their wire format and back, returning
/// the unified [`ChatResponse`] / [`StreamEvent`] types. `Raw` exposes the
/// provider-native response body as an escape hatch.
pub trait Provider: Clone + Send + Sync + 'static {
    /// Provider-native response body, preserved on [`ChatResponse::raw`].
    type Raw: Send + Sync;

    /// The model id this handle targets (used by the agent for the request and
    /// for cost lookup, so callers don't repeat the model).
    fn model_id(&self) -> &str;

    /// Performs a non-streamed completion.
    fn complete(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse<Self::Raw>, ProviderError>> + Send;

    /// Performs a streamed completion.
    fn stream(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<EventStream, ProviderError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_defaults() {
        let req = ChatRequest::new("claude-opus-4-8");
        assert_eq!(req.max_tokens, 4096);
        assert_eq!(req.tool_choice, ToolChoice::Auto);
        assert_eq!(req.cache, CacheStrategy::Auto);
        assert!(req.messages.is_empty());
    }

    #[test]
    fn tool_def_serializes_parameters_as_schema() {
        let def = ToolDef::new(
            "get_weather",
            "Get weather",
            serde_json::json!({ "type": "object" }),
        );
        let v = serde_json::to_value(&def).expect("serialize");
        assert_eq!(v["name"], "get_weather");
        assert_eq!(v["parameters"]["type"], "object");
    }

    // A trivial provider proving the trait is usable with native async fns.
    #[derive(Clone)]
    struct MockProvider;

    impl Provider for MockProvider {
        type Raw = ();

        #[expect(
            clippy::unnecessary_literal_bound,
            reason = "trait ties the lifetime to &self"
        )]
        fn model_id(&self) -> &str {
            "mock"
        }

        async fn complete(
            &self,
            _request: ChatRequest,
        ) -> Result<ChatResponse<Self::Raw>, ProviderError> {
            Ok(ChatResponse {
                content: vec![AssistantContent::Text("hello".into())],
                stop: StopReason::EndTurn,
                usage: Usage {
                    output_tokens: 1,
                    ..Usage::default()
                },
                raw: (),
            })
        }

        async fn stream(&self, _request: ChatRequest) -> Result<EventStream, ProviderError> {
            Err(ProviderError::InvalidArgument("not supported".into()))
        }
    }

    #[tokio::test]
    async fn mock_provider_completes() {
        let p = MockProvider;
        let resp = p
            .complete(ChatRequest::new("mock"))
            .await
            .expect("complete");
        assert_eq!(resp.content, vec![AssistantContent::Text("hello".into())]);
        assert_eq!(resp.stop, StopReason::EndTurn);
        assert_eq!(resp.usage.output_tokens, 1);
    }
}
