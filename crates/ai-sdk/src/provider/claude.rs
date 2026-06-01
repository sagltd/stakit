//! Anthropic Claude provider (Messages API).
//!
//! Maps the unified [`ChatRequest`] to the Anthropic wire format and back,
//! including streaming SSE: per-block argument fragments (`input_json_delta`)
//! are accumulated so a whole [`StreamEvent::ToolCall`] is emitted on
//! `content_block_stop`, and the cumulative `output_tokens` from `message_delta`
//! is taken as the running total (not added).

use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::cache::{CacheStrategy, CacheTarget};
use crate::message::{AssistantContent, Message, Thinking, ToolResultPart, UserContent};
use crate::provider::{
    ChatRequest, ChatResponse, EventStream, Provider, StopReason, StreamEvent, ThinkingConfig,
    ToolChoice, ToolDef,
};
use crate::usage::Usage;
use crate::{ProviderError, SystemPrompt};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A handle to the Anthropic API. Cheap to clone; mints per-model handles.
#[derive(Debug, Clone)]
pub struct ClaudeClient {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl ClaudeClient {
    /// Builds a client for the given API key (default endpoint).
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            http: reqwest::Client::new(),
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
    type Raw = Value;

    async fn complete(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse<Self::Raw>, ProviderError> {
        let body = build_body(&self.model, &request, false);
        let resp = self.send(body).await?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Decode {
            err: e.to_string(),
            body: text.clone(),
        })?;
        Ok(parse_response(value))
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, ProviderError> {
        let body = build_body(&self.model, &request, true);
        let resp = self.send(body).await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &text));
        }
        let mut bytes = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut buf = String::new();
            let mut accum = Accum::default();
            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::Transport(e.to_string()));
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(nl) = buf.find('\n') {
                    let line: String = buf.drain(..=nl).collect();
                    let line = line.trim_end();
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<Value>(data) {
                        for ev in accum.push(&event) {
                            yield Ok(ev);
                        }
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }
}

impl ClaudeModel {
    async fn send(&self, body: Value) -> Result<reqwest::Response, ProviderError> {
        self.client
            .http
            .post(format!("{}/v1/messages", self.client.base_url))
            .header("x-api-key", &self.client.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))
    }
}

fn api_error(status: u16, body: &str) -> ProviderError {
    let kind = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["type"].as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "api_error".to_owned());
    ProviderError::Api {
        status,
        kind,
        message: body.to_owned(),
    }
}

// --- request mapping -------------------------------------------------------

/// Builds the Anthropic request body from a unified [`ChatRequest`].
fn build_body(model: &str, req: &ChatRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(req.max_tokens));
    body.insert("stream".into(), json!(stream));
    body.insert("messages".into(), json!(map_messages(&req.messages)));

    let cache_tools = wants_cache(&req.cache, &CacheTarget::Tools)
        || (matches!(req.cache, CacheStrategy::Auto) && req.system.is_none());
    if let Some(system) = &req.system {
        body.insert(
            "system".into(),
            map_system(system, wants_cache(&req.cache, &CacheTarget::System)),
        );
    }
    if !req.tools.is_empty() {
        body.insert("tools".into(), json!(map_tools(&req.tools, cache_tools)));
        body.insert("tool_choice".into(), map_tool_choice(&req.tool_choice));
    }
    if let Some(temp) = req.temperature {
        body.insert("temperature".into(), json!(temp));
    }
    if !req.stop_sequences.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop_sequences));
    }
    match req.thinking {
        ThinkingConfig::Off => {}
        ThinkingConfig::Adaptive => {
            body.insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": 2048 }),
            );
        }
        ThinkingConfig::Budget(n) => {
            body.insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": n }),
            );
        }
    }
    for (k, v) in &req.extra {
        body.insert(k.clone(), v.clone());
    }
    Value::Object(body)
}

/// `Auto` caches the stable prefix (system if present, else tools); explicit
/// `Breakpoints` honor the requested targets.
fn wants_cache(strategy: &CacheStrategy, target: &CacheTarget) -> bool {
    match strategy {
        CacheStrategy::Off => false,
        CacheStrategy::Auto => matches!(target, CacheTarget::System),
        CacheStrategy::Breakpoints { points, .. } => points.contains(target),
    }
}

fn cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

fn map_system(system: &SystemPrompt, cache: bool) -> Value {
    if cache || system.cache {
        json!([{ "type": "text", "text": system.text, "cache_control": cache_control() }])
    } else {
        json!(system.text)
    }
}

fn map_tools(tools: &[ToolDef], cache_last: bool) -> Vec<Value> {
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
            if t.cache || (cache_last && i == last) {
                obj.insert("cache_control".into(), cache_control());
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

fn map_messages(messages: &[Message]) -> Vec<Value> {
    messages.iter().map(map_message).collect()
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
        ToolResultPart::Image(src) => json!({ "type": "image", "source": map_image(src) }),
    }
}

fn map_image(src: &crate::message::ImageSource) -> Value {
    match src {
        crate::message::ImageSource::Base64 { media_type, data } => json!({
            "type": "base64", "media_type": media_type, "data": data,
        }),
        crate::message::ImageSource::Url(url) => json!({ "type": "url", "url": url }),
    }
}

// --- response mapping ------------------------------------------------------

/// Parses a non-streamed Anthropic response into the unified shape. Missing or
/// unexpected fields degrade gracefully rather than erroring.
fn parse_response(value: Value) -> ChatResponse<Value> {
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
    let usage = value.get("usage").map(map_usage).unwrap_or_default();
    ChatResponse {
        content,
        stop,
        usage,
        raw: value,
    }
}

fn parse_assistant_block(block: &Value) -> Option<AssistantContent> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(AssistantContent::Text(
            block.get("text").and_then(Value::as_str)?.to_owned(),
        )),
        "tool_use" => Some(AssistantContent::ToolUse {
            id: block.get("id").and_then(Value::as_str)?.to_owned(),
            name: block.get("name").and_then(Value::as_str)?.to_owned(),
            input: block.get("input").cloned().unwrap_or_else(|| json!({})),
        }),
        "thinking" => Some(AssistantContent::Thinking(Thinking::Visible {
            text: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })),
        "redacted_thinking" => Some(AssistantContent::Thinking(Thinking::Redacted {
            data: block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
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
        cache_read_tokens: n("cache_read_input_tokens"),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("thinking_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

// --- streaming accumulator -------------------------------------------------

#[derive(Default)]
struct Accum {
    usage: Usage,
    tools: hashbrown::HashMap<u64, ToolBuf>,
    stop: Option<StopReason>,
}

struct ToolBuf {
    id: String,
    name: String,
    json: String,
}

impl Accum {
    fn push(&mut self, event: &Value) -> Vec<StreamEvent> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = event.get("message").and_then(|m| m.get("usage")) {
                    self.usage = map_usage(u);
                }
                vec![StreamEvent::Start { usage: self.usage }]
            }
            Some("content_block_start") => {
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        if let Some(idx) = event.get("index").and_then(Value::as_u64) {
                            self.tools.insert(
                                idx,
                                ToolBuf {
                                    id: block
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    name: block
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    json: String::new(),
                                },
                            );
                        }
                    }
                }
                Vec::new()
            }
            Some("content_block_delta") => self.on_delta(event),
            Some("content_block_stop") => {
                let idx = event.get("index").and_then(Value::as_u64);
                idx.and_then(|i| self.tools.remove(&i))
                    .map_or_else(Vec::new, |buf| {
                        let input = if buf.json.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(&buf.json).unwrap_or_else(|_| json!({}))
                        };
                        vec![StreamEvent::ToolCall {
                            id: buf.id,
                            name: buf.name,
                            input,
                        }]
                    })
            }
            Some("message_delta") => {
                if let Some(u) = event.get("usage") {
                    // output_tokens here is the cumulative running total.
                    let delta = map_usage(u);
                    self.usage.output_tokens = delta.output_tokens;
                    if delta.cache_read_tokens > 0 {
                        self.usage.cache_read_tokens = delta.cache_read_tokens;
                    }
                    if delta.reasoning_tokens > 0 {
                        self.usage.reasoning_tokens = delta.reasoning_tokens;
                    }
                }
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    let seq = event
                        .get("delta")
                        .and_then(|d| d.get("stop_sequence"))
                        .and_then(Value::as_str);
                    self.stop = Some(map_stop_reason(reason, seq));
                }
                Vec::new()
            }
            Some("message_stop") => vec![StreamEvent::End {
                stop: self.stop.clone().unwrap_or(StopReason::EndTurn),
                usage: self.usage,
            }],
            _ => Vec::new(),
        }
    }

    fn on_delta(&mut self, event: &Value) -> Vec<StreamEvent> {
        let Some(delta) = event.get("delta") else {
            return Vec::new();
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => delta
                .get("text")
                .and_then(Value::as_str)
                .map(|t| vec![StreamEvent::TextDelta(t.to_owned())])
                .unwrap_or_default(),
            Some("thinking_delta") => delta
                .get("thinking")
                .and_then(Value::as_str)
                .map(|t| vec![StreamEvent::ReasoningDelta(t.to_owned())])
                .unwrap_or_default(),
            Some("signature_delta") => delta
                .get("signature")
                .and_then(Value::as_str)
                .map(|s| vec![StreamEvent::SignatureDelta(s.to_owned())])
                .unwrap_or_default(),
            Some("input_json_delta") => {
                if let (Some(idx), Some(partial)) = (
                    event.get("index").and_then(Value::as_u64),
                    delta.get("partial_json").and_then(Value::as_str),
                ) {
                    if let Some(buf) = self.tools.get_mut(&idx) {
                        buf.json.push_str(partial);
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn thinking_config_maps_to_anthropic_thinking_budget() {
        use crate::provider::ThinkingConfig;
        let mut req = ChatRequest::new("claude-opus-4-8");
        req.thinking = ThinkingConfig::Budget(3000);
        let body = build_body("claude-opus-4-8", &req, false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 3000);

        req.thinking = ThinkingConfig::Off;
        let body = build_body("claude-opus-4-8", &req, false);
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
        let resp = parse_response(value);
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
            out.extend(accum.push(e));
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
                input: json!({ "city": "Paris" }),
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
        let evs = accum.push(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": { "type": "text_delta", "text": "Hel" }
        }));
        assert_eq!(evs, vec![StreamEvent::TextDelta("Hel".into())]);
    }
}
