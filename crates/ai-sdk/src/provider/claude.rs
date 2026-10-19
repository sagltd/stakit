//! Anthropic Claude provider (Messages API).
//!
//! Maps the unified [`ChatRequest`] to the Anthropic wire format and back,
//! including streaming SSE: per-block argument fragments (`input_json_delta`)
//! are accumulated so a whole [`crate::StreamEvent::ToolCall`] is emitted on
//! `content_block_stop`, and the cumulative `output_tokens` from `message_delta`
//! is taken as the running total (not added).

use serde_json::{Map, Value, json};

use crate::cache::{
    CacheCapabilities, CacheStrategy, CacheTarget, CacheTtl, ExplicitCacheCapabilities,
};
use crate::message::{AssistantContent, Image, Message, Thinking, ToolResultPart, UserContent};
use crate::provider::{
    ChatRequest, ChatResponse, EventStream, Provider, ProviderCapabilities, StopReason,
    ThinkingConfig, ToolChoice, ToolDef, parse_retry_after,
};
use crate::usage::Usage;
use crate::{ProviderError, SystemPrompt};

mod accum;
mod block;
#[cfg(test)]
#[path = "claude/cache_support_test.rs"]
mod cache_support_test;
mod models;
mod usage;
#[cfg(test)]
mod usage_presence_test;
use accum::Accum;
mod sse;

use models::{ThinkingOff, rules_for};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Upper bound on a single un-terminated SSE line. A stream that never sends a
/// newline would otherwise grow `buf` without bound; past this we bail with a
/// decode error rather than buffering unboundedly.
/// Max in-flight tool-call buffers a stream may open. A hostile stream could
/// otherwise allocate `content_block_start` entries without bound.
const MAX_INFLIGHT_TOOLS: usize = crate::provider::MAX_STREAM_OPEN_BLOCKS;
/// A handle to the Anthropic API. Cheap to clone; mints per-model handles.
#[derive(Clone)]
pub struct ClaudeClient {
    api_key: String,
    base_url: String,
    endpoint: Option<String>,
    http: Option<reqwest::Client>,
}

// Hand-written so the API key never leaks through `{:?}` (logs, panics, tests).
impl std::fmt::Debug for ClaudeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeClient")
            .field("api_key", &"[redacted]")
            .field("base_url", &self.base_url)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl ClaudeClient {
    /// Builds a client for the given API key (default endpoint).
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            endpoint: None,
            http: crate::provider::http_client(),
        }
    }

    /// Reads the key from `ANTHROPIC_API_KEY`.
    ///
    /// # Errors
    /// Returns [`ProviderError::InvalidArgument`] if the variable is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::InvalidArgument("ANTHROPIC_API_KEY is not set".into()))?;
        Ok(Self::new(key))
    }

    /// Overrides the base URL (e.g. a proxy or gateway).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Overrides the full request URL (instead of `{base_url}/v1/messages`), for
    /// compatible APIs mounted under a different path, e.g. Gemini's
    /// `https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`.
    #[must_use]
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into());
        self
    }

    /// Uses `http` for every request instead of the shared default client, e.g.
    /// one with a custom DNS resolver or proxy. The caller owns its retry and
    /// redirect policy.
    #[must_use]
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    fn url(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| format!("{}/v1/messages", self.base_url))
    }

    /// Mints a [`ClaudeModel`] for the given model id.
    #[must_use]
    pub fn model(&self, model: impl Into<String>) -> ClaudeModel {
        ClaudeModel {
            client: self.clone(),
            model: model.into(),
        }
    }
}

/// A specific Claude model, ready to serve [`Provider`] requests.
#[derive(Debug, Clone)]
pub struct ClaudeModel {
    client: ClaudeClient,
    model: String,
}

impl Provider for ClaudeModel {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        rules_for(&self.model).capabilities
    }

    fn capabilities_for(&self, model: &str) -> ProviderCapabilities {
        let model = if model.is_empty() { &self.model } else { model };
        rules_for(model).capabilities
    }

    fn cache_capabilities(&self) -> CacheCapabilities {
        CacheCapabilities::Known {
            implicit: false,
            explicit: Some(ExplicitCacheCapabilities {
                max_points: MAX_BREAKPOINTS,
                ttls: vec![CacheTtl::FiveMin, CacheTtl::OneHour],
            }),
        }
    }

    fn cache_capabilities_for(&self, _model: &str) -> CacheCapabilities {
        self.cache_capabilities()
    }

    fn complete(
        &self,
        mut request: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move {
            let model = self.pick_model(&request);
            request.fit_to(&rules_for(&model).capabilities);
            request.validate_cache(&self.cache_capabilities_for(&model))?;
            let body = build_body(&model, &request, false);
            let resp = self.send(body).await?;
            let status = resp.status();
            let retry_after = parse_retry_after(resp.headers());
            let text = resp
                .text()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            if !status.is_success() {
                return Err(api_error(status.as_u16(), &text, retry_after));
            }
            let value: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Decode {
                err: e.to_string(),
                body: text.clone(),
            })?;
            parse_response(&value)
        })
    }

    fn stream(
        &self,
        mut request: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            let model = self.pick_model(&request);
            request.fit_to(&rules_for(&model).capabilities);
            request.validate_cache(&self.cache_capabilities_for(&model))?;
            let body = build_body(&model, &request, true);
            let resp = self.send(body).await?;
            let status = resp.status();
            if !status.is_success() {
                let retry_after = parse_retry_after(resp.headers());
                let text = resp.text().await.unwrap_or_default();
                return Err(api_error(status.as_u16(), &text, retry_after));
            }
            Ok(sse::decode(resp.bytes_stream()))
        })
    }
}

impl ClaudeModel {
    /// The request's model id when set, else this handle's bound model.
    fn pick_model(&self, request: &ChatRequest) -> String {
        if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        }
    }

    async fn send(&self, body: Value) -> Result<reqwest::Response, ProviderError> {
        let http = self.client.http.as_ref().ok_or_else(|| {
            ProviderError::Transport("provider HTTP client initialization failed".into())
        })?;
        http.post(self.client.url())
            .header("x-api-key", &self.client.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))
    }
}

/// Builds an [`ProviderError::Api`] from a 4xx/5xx response.
///
/// Extracts only the provider's structured `error.type` / `error.message`; the
/// raw `body` is never stored, since it can echo back request material
/// (including the API key) and would surface via `Display` into the run
/// outcome. When the body lacks the expected shape, falls back to a concise,
/// safe message.
fn api_error(status: u16, body: &str, retry_after: Option<std::time::Duration>) -> ProviderError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let kind = parsed
        .as_ref()
        .and_then(|v| v["error"]["type"].as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "api_error".to_owned());
    let message = parsed
        .as_ref()
        .and_then(|v| v["error"]["message"].as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("provider error: {status} {kind}"));
    ProviderError::Api {
        status,
        kind,
        message,
        retry_after,
    }
}

// --- request mapping -------------------------------------------------------

/// Builds the Anthropic (non-streaming) request body for a [`ChatRequest`],
/// fitted to the request model's capabilities exactly as a real call is.
///
/// Exposed for offline body/cache-shape tests; the request's `model` field is
/// used (falling back to an empty model id), so callers need only pass the
/// request. Not part of the stable API.
#[doc(hidden)]
#[must_use]
pub fn build_request_body(req: &ChatRequest) -> Value {
    let mut fitted = req.clone();
    fitted.fit_to(&rules_for(&req.model).capabilities);
    build_body(&req.model, &fitted, false)
}

/// `xhigh` / `max` effort cannot run with thinking turned off (a 400), so
/// thinking-off leaves the model's adaptive thinking on there.
fn effort_requires_thinking(extra: &Map<String, Value>) -> bool {
    let effort = extra
        .get("output_config")
        .and_then(|config| config.get("effort"))
        .and_then(Value::as_str);
    matches!(effort, Some("xhigh" | "max"))
}

/// The `thinking` field for a fitted request, if any.
fn thinking_field(
    thinking: ThinkingConfig,
    off: ThinkingOff,
    extra: &Map<String, Value>,
) -> Option<Value> {
    match thinking {
        ThinkingConfig::Adaptive => Some(json!({ "type": "adaptive" })),
        ThinkingConfig::Budget(budget) => {
            Some(json!({ "type": "enabled", "budget_tokens": budget }))
        }
        ThinkingConfig::Off if effort_requires_thinking(extra) => None,
        ThinkingConfig::Off => match off {
            ThinkingOff::Omit => None,
            ThinkingOff::Disabled => Some(json!({ "type": "disabled" })),
            ThinkingOff::BetweenTools => Some(json!({ "type": "between_tools" })),
        },
    }
}

/// Builds the Anthropic request body from a unified [`ChatRequest`] already
/// fitted to the model's capabilities ([`ChatRequest::fit_to`]).
fn build_body(model: &str, req: &ChatRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(req.max_tokens));
    body.insert("stream".into(), json!(stream));

    // Resolve the cache breakpoints once (prefix order: tools → system →
    // messages), enforcing Anthropic's max of four across the whole request.
    let plan = CachePlan::resolve(req);
    let ttl = match req.cache {
        CacheStrategy::Off => None,
        CacheStrategy::Breakpoints { ttl, .. } => Some(ttl),
        CacheStrategy::Auto => Some(CacheTtl::FiveMin),
    };
    let mut cache_budget = MAX_BREAKPOINTS;
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            json!(map_tools(
                &req.tools,
                plan.cache_last_tool,
                ttl,
                &mut cache_budget
            )),
        );
        body.insert("tool_choice".into(), map_tool_choice(&req.tool_choice));
    }
    if let Some(system) = &req.system {
        body.insert(
            "system".into(),
            map_system(system, plan.cache_system, ttl, &mut cache_budget),
        );
    }
    body.insert(
        "messages".into(),
        json!(map_messages(
            &req.messages,
            &plan.message_breakpoints,
            ttl,
            &mut cache_budget
        )),
    );
    if let Some(temp) = req.temperature {
        body.insert("temperature".into(), json!(temp));
    }
    if !req.stop_sequences.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop_sequences));
    }
    if let Some(thinking) = thinking_field(req.thinking, rules_for(model).thinking_off, &req.extra)
    {
        body.insert("thinking".into(), thinking);
    }
    for (k, v) in &req.extra {
        body.insert(k.clone(), v.clone());
    }
    Value::Object(body)
}

/// Anthropic allows at most four `cache_control` breakpoints per request.
const MAX_BREAKPOINTS: usize = 4;

/// The resolved set of cache breakpoints for one request, in prefix order
/// (tools → system → messages). Computed once so the four-breakpoint cap is
/// enforced across the whole request rather than per block.
struct CachePlan {
    /// Place a breakpoint after the last tool definition.
    cache_last_tool: bool,
    /// Place a breakpoint after the system prompt block.
    cache_system: bool,
    /// Distinct message indices in prefix order, capped at the global maximum.
    message_breakpoints: Vec<usize>,
}

impl CachePlan {
    /// Resolves breakpoints for `req`, honoring the strategy and the global cap.
    fn resolve(req: &ChatRequest) -> Self {
        match &req.cache {
            CacheStrategy::Off => Self {
                cache_last_tool: false,
                cache_system: false,
                message_breakpoints: Vec::new(),
            },
            CacheStrategy::Auto => Self::auto(req),
            CacheStrategy::Breakpoints { points, .. } => Self::explicit(req, points),
        }
    }

    /// `Auto`: cache the shared prefix and the newest user/tool-result turn.
    /// The mapping functions share one budget, including author-forced flags.
    fn auto(req: &ChatRequest) -> Self {
        Self {
            cache_last_tool: !req.tools.is_empty(),
            cache_system: req.system.is_some(),
            message_breakpoints: last_user_message(&req.messages).into_iter().collect(),
        }
    }

    /// `Breakpoints`: place at exactly the requested targets, capped at four in
    /// prefix order (tools → system → messages).
    fn explicit(req: &ChatRequest, points: &[CacheTarget]) -> Self {
        let cache_last_tool = !req.tools.is_empty() && points.contains(&CacheTarget::Tools);
        let cache_system = req.system.is_some() && points.contains(&CacheTarget::System);

        // Resolve distinct targets in prefix order; duplicate requests consume
        // one slot and invalid indices consume none.
        let mut message_breakpoints = Vec::new();
        let last_user = last_user_message(&req.messages);
        for target in points {
            let idx = match target {
                CacheTarget::MessageIndex(i) if *i < req.messages.len() => Some(*i),
                CacheTarget::LastUserMessage => last_user,
                _ => None,
            };
            if let Some(i) = idx {
                message_breakpoints.push(i);
            }
        }
        message_breakpoints.sort_unstable();
        message_breakpoints.dedup();
        message_breakpoints.truncate(MAX_BREAKPOINTS);

        Self {
            cache_last_tool,
            cache_system,
            message_breakpoints,
        }
    }
}

/// Decrements `budget` and reports whether a breakpoint slot was available.
const fn take(budget: &mut usize) -> bool {
    if *budget == 0 {
        false
    } else {
        *budget -= 1;
        true
    }
}

/// The index of the last user message, if any.
fn last_user_message(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(|m| matches!(m, Message::User(_)))
}

fn cache_control(ttl: CacheTtl) -> Value {
    match ttl {
        CacheTtl::FiveMin => json!({ "type": "ephemeral" }),
        CacheTtl::OneHour => json!({ "type": "ephemeral", "ttl": "1h" }),
        // Public transport entry points reject this unsupported TTL. Keep the
        // requested value explicit in the offline body helper; never map to 5m.
        CacheTtl::ThirtyMin => json!({ "type": "ephemeral", "ttl": "30m" }),
    }
}

fn map_system(
    system: &SystemPrompt,
    cache: bool,
    ttl: Option<CacheTtl>,
    budget: &mut usize,
) -> Value {
    if let Some(ttl) = ttl
        && (cache || system.cache)
        && take(budget)
    {
        json!([{ "type": "text", "text": system.text, "cache_control": cache_control(ttl) }])
    } else {
        json!(system.text)
    }
}

fn map_tools(
    tools: &[ToolDef],
    cache_last: bool,
    ttl: Option<CacheTtl>,
    budget: &mut usize,
) -> Vec<Value> {
    let last = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut obj = Map::new();
            obj.insert("name".into(), json!(t.name));
            obj.insert("description".into(), json!(t.description));
            obj.insert("input_schema".into(), t.parameters.clone());
            if t.strict {
                obj.insert("strict".into(), json!(true));
            }
            if let Some(ttl) = ttl
                && (t.cache || (cache_last && i == last))
                && take(budget)
            {
                obj.insert("cache_control".into(), cache_control(ttl));
            }
            Value::Object(obj)
        })
        .collect()
}

fn map_tool_choice(choice: &ToolChoice) -> Value {
    // Parallel tool use is left enabled (the API default) in every arm — we
    // never send `disable_parallel_tool_use`, so the model may batch tool calls.
    match choice {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::Any | ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Tool(name) => json!({ "type": "tool", "name": name }),
    }
}

/// Maps the conversation, placing a rolling `cache_control` breakpoint on the
/// last content block of every selected message, within the shared budget.
fn map_messages(
    messages: &[Message],
    targets: &[usize],
    ttl: Option<CacheTtl>,
    budget: &mut usize,
) -> Vec<Value> {
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut mapped = map_message(m);
            if let Some(ttl) = ttl
                && targets.contains(&i)
            {
                mark_last_block(&mut mapped, ttl, budget);
            }
            mapped
        })
        .collect()
}

/// Attaches a cache breakpoint to the last content block of a mapped message.
fn mark_last_block(message: &mut Value, ttl: CacheTtl, budget: &mut usize) {
    if let Some(block) = message
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
        .and_then(Value::as_object_mut)
        && take(budget)
    {
        block.insert("cache_control".into(), cache_control(ttl));
    }
}

fn map_message(message: &Message) -> Value {
    match message {
        Message::User(parts) => {
            // Tool results must come first in an Anthropic user turn.
            let mut blocks: Vec<Value> = Vec::with_capacity(parts.len());
            for p in parts {
                if let UserContent::ToolResult {
                    id,
                    content,
                    is_error,
                } = p
                {
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "is_error": is_error,
                        "content": content.iter().map(map_tool_result_part).collect::<Vec<_>>(),
                    }));
                }
            }
            for p in parts {
                match p {
                    UserContent::Text(t) => blocks.push(json!({ "type": "text", "text": t })),
                    UserContent::Image(src) => {
                        blocks.push(json!({ "type": "image", "source": map_image(src) }));
                    }
                    UserContent::ToolResult { .. } => {}
                }
            }
            json!({ "role": "user", "content": blocks })
        }
        Message::Assistant(parts) => {
            let blocks: Vec<Value> = parts.iter().map(map_assistant_block).collect();
            json!({ "role": "assistant", "content": blocks })
        }
    }
}

fn map_assistant_block(block: &AssistantContent) -> Value {
    match block {
        AssistantContent::Text(t) => json!({ "type": "text", "text": t }),
        AssistantContent::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        AssistantContent::Thinking(Thinking::Visible { text, signature }) => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("thinking"));
            obj.insert("thinking".into(), json!(text));
            if let Some(sig) = signature {
                obj.insert("signature".into(), json!(sig));
            }
            Value::Object(obj)
        }
        AssistantContent::Thinking(Thinking::Redacted { data }) => {
            json!({ "type": "redacted_thinking", "data": data })
        }
    }
}

fn map_tool_result_part(part: &ToolResultPart) -> Value {
    match part {
        ToolResultPart::Text(t) => json!({ "type": "text", "text": t }),
        ToolResultPart::Image(img) => json!({ "type": "image", "source": map_image(img) }),
    }
}

fn map_image(img: &Image) -> Value {
    match img {
        Image::Url { url } => json!({ "type": "url", "url": &**url }),
        Image::Base64 { media_type, data } => {
            use base64::Engine as _;
            // note: base64 is re-encoded on every step when the same inline image
            // is replayed. Caching it would need an `OnceLock` inside
            // `Image::Base64`, which breaks the enum's `PartialEq`/`Eq`/`Serialize`
            // derives; prefer `Image::Url`/`Image::FileId` for repeated images.
            let b64 = base64::engine::general_purpose::STANDARD.encode(data);
            json!({ "type": "base64", "media_type": &**media_type, "data": b64 })
        }
        Image::FileId { file_id } => json!({ "type": "file", "file_id": &**file_id }),
    }
}

// --- response mapping ------------------------------------------------------

/// Parses a non-streamed Anthropic response into the unified shape. Missing or
/// unexpected content fields are ignored; mandatory usage must be observed.
fn parse_response(value: &Value) -> Result<ChatResponse, ProviderError> {
    let raw_usage = value
        .get("usage")
        .ok_or_else(|| sse::decode_error("Claude response requires usage"))?;
    usage::require_complete(raw_usage)?;
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().filter_map(parse_assistant_block).collect())
        .unwrap_or_default();
    let stop = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .map_or(StopReason::EndTurn, |s| {
            map_stop_reason(s, value.get("stop_sequence").and_then(Value::as_str))
        });
    let usage = map_usage(raw_usage);
    Ok(ChatResponse {
        content,
        stop,
        usage,
    })
}

fn parse_assistant_block(block: &Value) -> Option<AssistantContent> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(AssistantContent::Text(
            block.get("text").and_then(Value::as_str)?.into(),
        )),
        "tool_use" => Some(AssistantContent::ToolUse {
            id: block.get("id").and_then(Value::as_str)?.into(),
            name: block.get("name").and_then(Value::as_str)?.into(),
            input: block.get("input").cloned().unwrap_or_else(|| json!({})),
        }),
        "thinking" => Some(AssistantContent::Thinking(Thinking::Visible {
            text: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .map(Into::into),
        })),
        "redacted_thinking" => Some(AssistantContent::Thinking(Thinking::Redacted {
            data: block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        })),
        _ => None,
    }
}

fn map_stop_reason(reason: &str, stop_sequence: Option<&str>) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence(stop_sequence.unwrap_or_default().to_owned()),
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::Pause,
        other => StopReason::Other(other.to_owned()),
    }
}

fn map_usage(usage: &Value) -> Usage {
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_create_tokens: n("cache_creation_input_tokens"),
        cache_create_1h_tokens: usage
            .get("cache_creation")
            .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: n("cache_read_input_tokens"),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("thinking_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamEvent;
    use crate::message::UserContent;

    #[test]
    fn build_body_sets_core_fields_and_caches_system() {
        let mut req = ChatRequest::new("claude-opus-4-8");
        req.system = Some(SystemPrompt::from("be helpful"));
        req.messages = vec![Message::user_text("hi")];
        let body = build_body("claude-opus-4-8", &req, false);
        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], false);
        // Auto caching puts a breakpoint on the system block.
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn thinking_config_maps_to_the_model_thinking_shape() {
        let mut req = ChatRequest::new("claude-haiku-4-5-20251001");
        req.thinking = ThinkingConfig::Budget(3000);
        let body = build_request_body(&req);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 3000);

        // Opus 4.8 rejects budgets: the same request is sent as adaptive.
        req.model = "claude-opus-4-8".into();
        let body = build_request_body(&req);
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));

        req.thinking = ThinkingConfig::Off;
        let body = build_request_body(&req);
        assert!(body.get("thinking").is_none(), "Off must omit thinking");
    }

    #[test]
    fn tool_result_blocks_come_first_in_user_turn() {
        let msg = Message::User(vec![
            UserContent::Text("here".into()),
            UserContent::ToolResult {
                id: "toolu_1".into(),
                content: vec![ToolResultPart::Text("42".into())],
                is_error: false,
            },
        ]);
        let v = map_message(&msg);
        assert_eq!(v["content"][0]["type"], "tool_result");
        assert_eq!(v["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(v["content"][1]["type"], "text");
    }

    #[test]
    fn parse_response_reads_text_tooluse_stop_and_usage() {
        let value = json!({
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "tool_use", "id": "toolu_9", "name": "wx", "input": { "city": "Paris" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 7, "cache_read_input_tokens": 3 }
        });
        let resp = parse_response(&value).expect("valid response");
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.usage.cache_read_tokens, 3);
        assert_eq!(
            resp.content[1],
            AssistantContent::ToolUse {
                id: "toolu_9".into(),
                name: "wx".into(),
                input: json!({ "city": "Paris" }),
            }
        );
    }

    #[test]
    fn accumulator_emits_whole_tool_call_from_fragments() {
        let mut accum = Accum::default();
        let events = [
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 5 } } }),
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_1", "name": "wx", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"city\":" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": "\"Paris\"}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" },
                    "usage": { "output_tokens": 12 } }),
            json!({ "type": "message_stop" }),
        ];
        let mut out = Vec::new();
        for e in &events {
            out.extend(accum.push(e).expect("push ok"));
        }
        assert!(matches!(out[0], StreamEvent::Start { .. }));
        let tool_call = out
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolCall { .. }))
            .expect("tool call emitted");
        assert_eq!(
            *tool_call,
            StreamEvent::ToolCall {
                id: "toolu_1".into(),
                name: "wx".into(),
                arguments: "{\"city\":\"Paris\"}".to_owned(),
            }
        );
        let StreamEvent::End { stop, usage } = out.last().expect("end") else {
            panic!("last event is not End");
        };
        assert_eq!(*stop, StopReason::ToolUse);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.input_tokens, 5);
    }

    #[test]
    fn accumulator_streams_text() {
        let mut accum = Accum::default();
        // A delta belongs to an opened block; complete-block replay must retain
        // that same block rather than relying on an orphaned UI fragment.
        accum
            .push(&json!({ "type":"content_block_start", "index":0,
            "content_block":{"type":"text", "text":""} }))
            .expect("start");
        let evs = accum
            .push(&json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "Hel" }
            }))
            .expect("push ok");
        assert_eq!(evs, vec![StreamEvent::TextDelta("Hel".into())]);
    }

    #[test]
    fn accumulator_caps_inflight_tool_count() {
        let mut accum = Accum::default();
        // Opening MAX_INFLIGHT_TOOLS distinct tool blocks is fine.
        for idx in 0..MAX_INFLIGHT_TOOLS as u64 {
            let ev = json!({ "type": "content_block_start", "index": idx,
                "content_block": { "type": "tool_use", "id": format!("t{idx}"), "name": "wx", "input": {} } });
            accum.push(&ev).expect("under cap");
        }
        // One more distinct tool index must trip the decode error.
        let over = json!({ "type": "content_block_start", "index": MAX_INFLIGHT_TOOLS as u64,
            "content_block": { "type": "tool_use", "id": "tX", "name": "wx", "input": {} } });
        let err = accum.push(&over).expect_err("over cap must error");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[test]
    fn accumulator_caps_single_tool_arg_buffer() {
        let mut accum = Accum::default();
        accum
            .push(&json!({ "type": "content_block_start", "index": 0,
                "content_block": { "type": "tool_use", "id": "t0", "name": "wx", "input": {} } }))
            .expect("start ok");
        // A single oversized argument fragment must trip the decode error.
        let huge = "x".repeat(crate::provider::MAX_STREAM_BLOCK_BYTES + 1);
        let err = accum
            .push(&json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": huge } }))
            .expect_err("oversized args must error");
        assert!(matches!(err, ProviderError::Decode { .. }), "{err:?}");
    }

    #[test]
    fn api_error_does_not_surface_raw_body_secrets() {
        // The raw body echoes back request material (here a fake key); only the
        // provider's structured error.message must reach Display.
        let body = json!({
            "error": {
                "type": "invalid_request_error",
                "message": "model not found"
            },
            "request_echo": { "x-api-key": "sk-secret-LEAK" }
        })
        .to_string();
        let err = api_error(400, &body, None);
        let surfaced = err.to_string();
        assert!(
            !surfaced.contains("sk-secret-LEAK"),
            "raw body leaked into Display: {surfaced}"
        );
        assert!(surfaced.contains("model not found"), "{surfaced}");
    }

    #[test]
    fn api_error_falls_back_to_safe_message_for_opaque_body() {
        // An HTML/plain body (no JSON error.message) must not be surfaced verbatim.
        let err = api_error(502, "<html>sk-secret-LEAK gateway</html>", None);
        let surfaced = err.to_string();
        assert!(!surfaced.contains("sk-secret-LEAK"), "{surfaced}");
        assert!(surfaced.contains("502"), "{surfaced}");
    }

    #[test]
    fn client_debug_redacts_api_key() {
        let client = ClaudeClient::new("sk-secret-LEAK");
        let dbg = format!("{client:?}");
        assert!(!dbg.contains("sk-secret-LEAK"), "{dbg}");
        assert!(dbg.contains("[redacted]"), "{dbg}");
        // The model handle embeds the client and must inherit redaction.
        let model = client.model("claude-opus-4-8");
        let dbg = format!("{model:?}");
        assert!(!dbg.contains("sk-secret-LEAK"), "{dbg}");
    }
}
