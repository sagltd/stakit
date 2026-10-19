//! One-shot LLM calls (no agent loop): structured extraction or plain text.
//!
//! [`LLM`] wraps a [`Provider`] and exposes one-shot terminal methods:
//! - [`LLM::extract`] — requests a single tool call and deserialises the input
//!   into a typed `T: JsonSchema + DeserializeOwned`.
//! - [`LLM::extract_with_usage`] — retains observed accounting metadata even when
//!   extraction fails, and validates the decoded value.
//! - [`LLM::text`] — collects all assistant text blocks and concatenates them.
//!
//! No method starts an agent loop or retries. Each fires one [`Provider::complete`]
//! call after request preflight succeeds.

use std::sync::Arc;

mod extraction;
pub use extraction::{Extraction, ExtractionError};

use crate::{
    AssistantContent, CacheStrategy, Provider, SystemPrompt,
    error::AgentError,
    message::Message,
    provider::{ChatRequest, ToolChoice, ToolDef},
};

/// A single-turn LLM call helper, generic over a [`Provider`].
///
/// Build with [`LLM::new`], chain setter methods, then call [`LLM::extract`]
/// or [`LLM::text`] to fire the request.
pub struct LLM<P: Provider> {
    provider: P,
    model: Option<String>,
    system: Option<String>,
    user: Option<Arc<str>>,
    max_tokens: u32,
    temperature: Option<f32>,
    cache: CacheStrategy,
}

impl<P: Provider> LLM<P> {
    /// Creates a new helper backed by `provider`.
    pub const fn new(provider: P) -> Self {
        Self {
            provider,
            model: None,
            system: None,
            user: None,
            max_tokens: 1024,
            temperature: None,
            cache: CacheStrategy::Auto,
        }
    }

    /// Overrides the model id (default: the provider's [`Provider::model_id`]).
    #[must_use]
    pub fn model(mut self, m: impl Into<String>) -> Self {
        self.model = Some(m.into());
        self
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = Some(s.into());
        self
    }

    /// Sets the user message text.
    ///
    /// If not set, [`LLM::extract`] and [`LLM::text`] will return
    /// [`AgentError::Schema`] rather than sending an empty user turn.
    #[must_use]
    pub fn user(mut self, u: impl Into<Arc<str>>) -> Self {
        self.user = Some(u.into());
        self
    }

    /// Sets the maximum number of output tokens (default: `1024`).
    #[must_use]
    pub const fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub const fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }

    /// Sets prompt-cache controls (default: [`CacheStrategy::Auto`]).
    /// Unsupported explicit controls fail before the provider call.
    #[must_use]
    pub fn cache(mut self, cache: CacheStrategy) -> Self {
        self.cache = cache;
        self
    }

    /// Builds the base [`ChatRequest`] shared by all terminal methods.
    ///
    /// Returns an error when no user message has been set.
    fn build_request(
        &self,
        tools: Arc<[ToolDef]>,
        tool_choice: ToolChoice,
    ) -> Result<ChatRequest, AgentError> {
        let user_text = self.user.clone().ok_or_else(|| {
            AgentError::Schema("LLM: user message must be set before calling extract/text".into())
        })?;

        let model = self
            .model
            .clone()
            .unwrap_or_else(|| self.provider.model_id().to_string());

        let system: Option<SystemPrompt> = self.system.as_deref().map(SystemPrompt::from);

        let mut req = ChatRequest::new(model);
        req.system = system;
        req.messages = vec![Message::user(user_text)];
        req.tools = tools;
        req.tool_choice = tool_choice;
        req.max_tokens = self.max_tokens;
        req.temperature = self.temperature;
        req.cache = self.cache.clone();
        req.fit_to(&self.provider.capabilities_for(&req.model));
        req.validate_cache(&self.provider.cache_capabilities_for(&req.model))?;
        Ok(req)
    }

    fn extraction_request<T: stakit_model::JsonSchema>(&self) -> Result<ChatRequest, AgentError> {
        let tool = ToolDef::new("extract", "Return the structured result.", T::schema());
        let mut req = self.build_request(Arc::from([tool]), ToolChoice::Tool("extract".into()))?;
        if req.tool_choice == ToolChoice::Auto {
            let instruction = "Return the structured result by calling the extract tool. \
                Do not answer with prose or a JSON text block.";
            let system = req.system.get_or_insert_with(|| SystemPrompt::from(""));
            system.text = if system.text.is_empty() {
                instruction.into()
            } else {
                format!("{}\n\n{instruction}", system.text).into()
            };
        }

        Ok(req)
    }

    /// Fires one request and returns a structured `T` using a synthetic tool
    /// whose input schema is `T::schema()`.
    ///
    /// Forces the tool when the provider supports it. Otherwise uses auto tool
    /// choice and an explicit system instruction. Both paths require an actual
    /// `extract` tool-use block and deserialize its input into `T`; prose is
    /// never accepted as a structured result. No retries or extra calls are made.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Schema`] when the user message is absent, when
    /// the provider response contains no `"extract"` tool-use block, or when
    /// the tool's `input` cannot be deserialised into `T`.
    pub async fn extract<T: stakit_model::JsonSchema + serde::de::DeserializeOwned>(
        self,
    ) -> Result<T, AgentError> {
        let req = self.extraction_request::<T>()?;

        let resp = self.provider.complete(req).await?;

        let input = extraction::tool_input(&resp.stop, resp.content, false).map_err(|error| {
            let message = match error {
                ExtractionError::Refused | ExtractionError::Incomplete => {
                    "LLM::extract: provider refused or did not finish the structured result"
                }
                _ => "LLM::extract: provider response contained no 'extract' tool-use block",
            };
            AgentError::Schema(message.into())
        })?;

        serde_json::from_value::<T>(input).map_err(|e| AgentError::Schema(e.to_string()))
    }

    /// Fires one request (no tools) and returns the concatenation of all
    /// assistant text blocks in the response.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Schema`] when the user message is absent, or
    /// [`AgentError::Provider`] on a provider failure.
    pub async fn text(self) -> Result<String, AgentError> {
        let req = self.build_request(Arc::from([]), ToolChoice::None)?;

        let resp = self.provider.complete(req).await?;

        let text = resp
            .content
            .into_iter()
            .filter_map(|block| match block {
                AssistantContent::Text(t) => Some(t.to_string()),
                _ => None,
            })
            .collect::<String>();

        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use futures::future::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::provider::tests::MockProvider;
    use crate::{
        ProviderError,
        provider::{ChatResponse, EventStream, StopReason},
        usage::Usage,
    };

    // ── Mock for extract ────────────────────────────────────────────────────

    struct ExtractMock;

    impl Provider for ExtractMock {
        #[allow(
            clippy::unnecessary_literal_bound,
            reason = "trait method must return &str"
        )]
        fn model_id(&self) -> &str {
            "mock-extract"
        }

        fn complete(
            &self,
            _req: ChatRequest,
        ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
            Box::pin(async {
                Ok(ChatResponse {
                    content: vec![AssistantContent::ToolUse {
                        id: "call_1".into(),
                        name: "extract".into(),
                        input: json!({ "name": "Bob", "age": 30 }),
                    }],
                    stop: StopReason::ToolUse,
                    usage: Usage {
                        output_tokens: 10,
                        ..Usage::default()
                    },
                })
            })
        }

        fn stream(&self, _req: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
            unimplemented!("ExtractMock does not support streaming")
        }
    }

    // ── Test structs ─────────────────────────────────────────────────────────

    #[derive(Debug, serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
    struct User {
        /// The user's name.
        name: String,
        /// The user's age.
        age: u32,
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn llm_extract_returns_typed_value() {
        let u: User = LLM::new(ExtractMock)
            .system("x")
            .user("Bob is 30")
            .extract::<User>()
            .await
            .unwrap();
        assert_eq!(u.name, "Bob");
        assert_eq!(u.age, 30);
    }

    #[tokio::test]
    async fn llm_text_returns_string() {
        let s = LLM::new(MockProvider).user("hi").text().await.unwrap();
        assert_eq!(s, "hello");
    }

    #[tokio::test]
    async fn llm_missing_user_returns_schema_error() {
        let err = LLM::new(MockProvider).text().await.unwrap_err();
        assert!(matches!(err, AgentError::Schema(_)));
    }

    #[tokio::test]
    async fn llm_extract_missing_tool_use_returns_schema_error() {
        // MockProvider returns a Text block, not a ToolUse block.
        let err = LLM::new(MockProvider)
            .user("test")
            .extract::<User>()
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Schema(_)));
    }
}
