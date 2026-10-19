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

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AssistantContent, CacheCapabilities, CacheStrategy, Message, ProviderError, SystemPrompt, Usage,
};

mod capabilities;
#[cfg(feature = "claude")]
pub(crate) mod claude;
#[cfg(any(feature = "claude", feature = "openai"))]
mod http;
#[cfg(feature = "openai")]
pub(crate) mod openai;
#[cfg(any(feature = "claude", feature = "openai"))]
mod sse;
#[cfg(all(test, any(feature = "claude", feature = "openai")))]
mod test_util;

pub use capabilities::{ProviderCapabilities, ThinkingSupport};

/// Maximum bytes retained for one complete streamed assistant block.
pub(crate) const MAX_STREAM_BLOCK_BYTES: usize = 1024 * 1024;
/// Maximum complete assistant content retained for one response.
pub(crate) const MAX_STREAM_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum concurrently open provider content blocks.
#[cfg(any(feature = "claude", feature = "openai"))]
pub(crate) const MAX_STREAM_OPEN_BLOCKS: usize = 64;
/// Maximum complete blocks in one response, including empty blocks.
pub(crate) const MAX_STREAM_RESPONSE_BLOCKS: usize = 1024;

/// Exact content bytes retained for replay (including opaque signatures).
pub(crate) fn assistant_content_bytes(content: &AssistantContent) -> usize {
    match content {
        AssistantContent::Text(text) => text.len(),
        AssistantContent::Thinking(crate::Thinking::Visible { text, signature }) => text
            .len()
            .saturating_add(signature.as_ref().map_or(0, |value| value.len())),
        AssistantContent::Thinking(crate::Thinking::Redacted { data }) => data.len(),
        AssistantContent::ToolUse { id, name, input } => id
            .len()
            .saturating_add(name.len())
            .saturating_add(input.to_string().len()),
    }
}

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

/// Extended-thinking / reasoning configuration, fitted to each model's
/// [`ThinkingSupport`] before sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThinkingConfig {
    /// No extended thinking, where the model allows it. Models whose thinking
    /// cannot be turned off (Claude Opus 5.5, Fable, Mythos) run at their
    /// lowest setting instead; lower the effort there (`output_config.effort`
    /// through [`ChatRequest::extra`]) to think less.
    #[default]
    Off,
    /// The model decides how much to think (adaptive thinking).
    Adaptive,
    /// An explicit token budget (budget-style models; adaptive models treat it
    /// as [`ThinkingConfig::Adaptive`]).
    Budget(u32),
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StopReason {
    /// Natural end of the assistant turn.
    #[default]
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
    ///
    /// An `Arc<[ToolDef]>` so the agent loop can hoist the (run-constant) tool
    /// set once and clone a pointer into each step's request instead of
    /// deep-copying the `Vec` every step.
    pub tools: Arc<[ToolDef]>,
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
    /// Opaque cache-routing key for this conversation. Providers with
    /// key-routed caching (e.g. `OpenAI`'s `prompt_cache_key`) can use it to
    /// scope cache accounting and, on some models, influence routing. Cache hits
    /// still depend on provider capacity and prefix matching. `None` uses the
    /// provider's default routing and accounting scope.
    pub cache_key: Option<Arc<str>>,
    /// Raw per-provider passthrough merged into the request body.
    pub extra: serde_json::Map<String, Value>,
}

impl ChatRequest {
    /// Validates explicit cache controls against the selected model's adapter.
    ///
    /// # Errors
    /// Returns [`ProviderError::InvalidArgument`] instead of silently changing
    /// unsupported explicit cache modes or lifetimes.
    pub fn validate_cache(&self, capabilities: &CacheCapabilities) -> Result<(), ProviderError> {
        capabilities.validate(&self.cache)
    }

    /// A request for `model` with sensible defaults (4096 max tokens, auto
    /// tool-choice, auto caching).
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Arc::from([]),
            tool_choice: ToolChoice::Auto,
            max_tokens: 4096,
            temperature: None,
            stop_sequences: Vec::new(),
            thinking: ThinkingConfig::Off,
            cache: CacheStrategy::Auto,
            cache_key: None,
            extra: serde_json::Map::new(),
        }
    }
}

/// A non-streamed provider response.
///
/// A successful response contains observed, final mandatory input and output
/// usage for all returned content. Missing or partial counters are a provider
/// error, not a zero-cost response. An explicit zero is valid only when it was
/// actually observed from the backend.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Normalized assistant output blocks.
    pub content: Vec<AssistantContent>,
    /// Why generation stopped.
    pub stop: StopReason,
    /// Observed final normalized usage for this entire request.
    pub usage: Usage,
}

/// A normalized streaming event.
///
/// Start and progress tallies may be partial. Only a valid terminal
/// [`StreamEvent::End`] certifies final usage; a default or synthetic zero tally
/// does not establish that the backend reported zero usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// Stream opened; carries the initial (input) usage where available.
    Start {
        /// Partial usage known at start (e.g. input tokens).
        usage: Usage,
    },
    /// Latest cumulative usage known during this response. Progress metadata
    /// does not complete the response or authorize queued tools. Providers
    /// retain previously known counts when a wire delta omits those fields.
    Usage {
        /// Full normalized tally so far; replaces, rather than adds to, prior usage.
        usage: Usage,
    },
    /// A chunk of assistant text.
    TextDelta(String),
    /// A chunk of reasoning text.
    ReasoningDelta(String),
    /// A reasoning integrity signature chunk (Anthropic).
    SignatureDelta(String),
    /// One complete assistant block, with its original response position.
    ///
    /// Providers emitting these events must supply every replay block. Deltas
    /// still drive live output; complete blocks preserve exact ordering and
    /// integrity signatures, including empty signed and redacted reasoning.
    AssistantBlock {
        /// Original zero-based position in the assistant response.
        index: u64,
        /// Complete replayable content, unchanged from the provider.
        content: AssistantContent,
    },
    /// A complete tool call (argument fragments are accumulated internally).
    ///
    /// This queues a candidate call; it does not authorize tool execution. The
    /// agent must wait for a valid terminal [`StreamEvent::End`].
    ToolCall {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Raw, unparsed JSON argument text exactly as the model emitted it.
        ///
        /// Parsing is deferred to the agent's dispatch so a single site decides
        /// what malformed arguments mean (a tool error the model can retry),
        /// rather than each provider silently coercing them to `{}`.
        arguments: String,
    },
    /// Stream finished; carries the stop reason and final cumulative usage.
    ///
    /// Every content block must be closed, and this tally must cover all content
    /// emitted by this response, including tool arguments and reasoning where
    /// the backend counts them. Mandatory input and output counters must be
    /// observed and final. Emit an error instead of this event when that cannot
    /// be established; an EOF, partial tally or synthetic zero is not completion.
    /// No content or tool-call events may follow this event.
    End {
        /// Why generation stopped.
        stop: StopReason,
        /// Final usage for the request.
        usage: Usage,
    },
}

/// A boxed stream of streaming events.
pub type EventStream = futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>;

/// Builds an [`EventStream`] from a ready list of events.
///
/// Convenience for [`Provider`] implementors (e.g. non-streaming backends, or
/// tests) so they can return a stream without depending on `futures` directly.
#[must_use]
pub fn event_stream(events: Vec<Result<StreamEvent, ProviderError>>) -> EventStream {
    use futures::StreamExt as _;
    futures::stream::iter(events).boxed()
}

/// The HTTP client the built-in providers share: a bounded connect, and a
/// per-read timeout so a connection that stalls mid-stream ends with an error
/// instead of hanging forever (streams send keep-alive pings well within it;
/// generous enough for a long non-streamed completion).
#[cfg(any(feature = "claude", feature = "openai"))]
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        // The application owns attempt authorization, including protocol NACKs.
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(300))
}

#[cfg(any(feature = "claude", feature = "openai"))]
pub(crate) fn http_client() -> Option<reqwest::Client> {
    // Never replace a failed configured client with one that restores retries.
    http_client_builder().build().ok()
}

#[cfg(all(test, any(feature = "claude", feature = "openai")))]
#[path = "provider/http_client_test.rs"]
mod http_client_test;

#[cfg(all(test, feature = "claude", feature = "openai"))]
mod redirect_test;

#[cfg(all(test, feature = "claude", feature = "openai"))]
#[path = "provider/endpoint_test.rs"]
mod endpoint_test;

/// Parses the `Retry-After` response header (delta-seconds form) into a
/// [`Duration`](std::time::Duration). HTTP-date forms and unparseable values
/// yield `None`. Shared by the built-in providers when building a 429 error.
#[cfg(any(feature = "claude", feature = "openai"))]
pub(crate) fn parse_retry_after(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::Duration::from_secs(secs))
}

/// A chat-completion backend. Object-safe so the agent can hold
/// `Box<dyn Provider>` and switch providers at runtime.
///
/// A new backend maps the unified request/response to its wire format and
/// declares [`capabilities`](Provider::capabilities) and implemented
/// [`cache_capabilities`](Provider::cache_capabilities); shared callers use the
/// request-model variants. Adapters also call [`ChatRequest::validate_cache`]
/// before direct `complete`/`stream` transport so unsupported explicit controls
/// cannot be silently dropped. `test_support::contract` checks the promises
/// the agent loop relies on.
///
/// # Completion and usage contract
///
/// Built-in and custom implementations must return [`ChatResponse`] success or
/// emit [`StreamEvent::End`] only after observing final mandatory input and
/// output counters for the entire response. Normalize counters into [`Usage`]
/// without inventing missing values. Explicit zero counters are accepted only
/// as actual backend evidence; partial progress and [`Usage::default()`] do not
/// prove final zero usage. If final counters or stream completion are missing,
/// return a [`ProviderError`] and preserve any already emitted progress instead
/// of certifying success. The agent must not execute queued tools before a valid
/// terminal `End`. These obligations do not require additional public flags.
///
/// Final counters need not all appear in the last wire frame: earlier observed
/// input may be retained when the backend protocol establishes it is final for
/// the whole response. Final output must cover the last emitted content; an
/// earlier partial output tally cannot stand in for that final evidence.
///
/// The numeric [`Usage`] value alone cannot prove where its counts came from;
/// custom adapters are responsible for enforcing this boundary.
pub trait Provider: Send + Sync {
    /// The model id this provider serves (registry key).
    fn model_id(&self) -> &str;

    /// What this provider's model accepts. The agent loop and [`LLM`](crate::LLM)
    /// fit every request to it ([`ChatRequest::fit_to`]); the default takes the
    /// unified request as it is (see [`ProviderCapabilities::default`]).
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Capabilities for the model selected on a request. Providers that allow
    /// overriding their bound model should resolve that model here; fixed-model
    /// providers can keep the default. Shared callers use this method so an
    /// [`LLM::model`](crate::LLM::model) override cannot apply the wrong rules.
    fn capabilities_for(&self, _model: &str) -> ProviderCapabilities {
        self.capabilities()
    }

    /// Cache controls implemented for the bound model. The default preserves
    /// source compatibility while making no unsupported caching guarantees.
    fn cache_capabilities(&self) -> CacheCapabilities {
        CacheCapabilities::Unknown
    }

    /// Cache controls for the actual request model (including model overrides).
    fn cache_capabilities_for(&self, _model: &str) -> CacheCapabilities {
        self.cache_capabilities()
    }

    /// One non-streaming completion with observed final usage on success.
    fn complete(
        &self,
        req: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<ChatResponse, ProviderError>>;

    /// A streaming completion obeying the terminal [`StreamEvent::End`] contract.
    fn stream(
        &self,
        req: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<EventStream, ProviderError>>;
}

#[cfg(test)]
pub(crate) mod tests {
    use futures::future::BoxFuture;

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

    /// A trivial provider used in unit tests: a canned `"hello"` text
    /// completion and an empty, well-terminated stream.
    pub(crate) struct MockProvider;

    impl Provider for MockProvider {
        #[allow(
            clippy::unnecessary_literal_bound,
            reason = "trait method must return &str"
        )]
        fn model_id(&self) -> &str {
            "mock"
        }

        fn complete(
            &self,
            _req: ChatRequest,
        ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
            Box::pin(async move {
                Ok(ChatResponse {
                    content: vec![AssistantContent::Text("hello".into())],
                    stop: StopReason::EndTurn,
                    usage: Usage {
                        output_tokens: 1,
                        ..Usage::default()
                    },
                })
            })
        }

        fn stream(&self, _req: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
            Box::pin(async move {
                Ok(event_stream(vec![
                    Ok(StreamEvent::Start {
                        usage: Usage::default(),
                    }),
                    Ok(StreamEvent::End {
                        stop: StopReason::EndTurn,
                        usage: Usage::default(),
                    }),
                ]))
            })
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

    #[tokio::test]
    async fn provider_is_object_safe_and_dispatches_via_dyn() {
        let p: Box<dyn Provider> = Box::new(MockProvider);
        assert_eq!(p.model_id(), "mock");
        let _ = p.stream(ChatRequest::new("mock")).await;
    }
}
