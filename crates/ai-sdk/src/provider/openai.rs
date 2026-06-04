//! `OpenAI` provider (Chat Completions API).
//!
//! Maps the unified [`ChatRequest`] to `OpenAI`'s Chat Completions wire format and
//! back. Tool results become separate `role: "tool"` messages; assistant tool
//! calls live in a `tool_calls` array with stringified JSON arguments (parsed on
//! ingest). Streaming accumulates per-index `tool_calls` argument fragments and
//! emits a whole [`StreamEvent::ToolCall`] when the call finishes; usage arrives
//! in a trailing chunk via `stream_options.include_usage`.

use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::message::{AssistantContent, Image, Message, ToolResultPart, UserContent};
use crate::provider::{
    ChatRequest, ChatResponse, EventStream, Provider, StopReason, StreamEvent, ToolChoice, ToolDef,
};
use crate::usage::Usage;
use crate::{ProviderError, SystemPrompt};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// A handle to the `OpenAI` API. Cheap to clone; mints per-model handles.
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl OpenAiClient {
    /// Builds a client for the given API key (default endpoint).
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// Reads the key from `OPENAI_API_KEY`.
    ///
    /// # Errors
    /// Returns [`ProviderError::InvalidArgument`] if the variable is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ProviderError::InvalidArgument("OPENAI_API_KEY is not set".into()))?;
        Ok(Self::new(key))
    }

    /// Overrides the base URL (e.g. an `OpenAI`-compatible endpoint).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Mints an [`OpenAiModel`] for the given model id.
    #[must_use]
    pub fn model(&self, model: impl Into<String>) -> OpenAiModel {
        OpenAiModel {
            client: self.clone(),
            model: model.into(),
        }
    }
}

/// A specific `OpenAI` model (Chat Completions).
#[derive(Debug, Clone)]
pub struct OpenAiModel {
    client: OpenAiClient,
    model: String,
}

impl Provider for OpenAiModel {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete(
        &self,
        request: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move {
            let body = build_body(&self.pick_model(&request), &request, false);
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
            Ok(parse_response(&value))
        })
    }

    fn stream(
        &self,
        request: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            let body = build_body(&self.pick_model(&request), &request, true);
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
                let mut started = false;
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
                        let line = line.trim();
                        let Some(data) = line.strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data.is_empty() {
                            continue;
                        }
                        if data == "[DONE]" {
                            for ev in accum.finish() {
                                yield Ok(ev);
                            }
                            return;
                        }
                        if let Ok(event) = serde_json::from_str::<Value>(data) {
                            if !started {
                                started = true;
                                yield Ok(StreamEvent::Start { usage: Usage::default() });
                            }
                            for ev in accum.push(&event) {
                                yield Ok(ev);
                            }
                        }
                    }
                }
                for ev in accum.finish() {
                    yield Ok(ev);
                }
            };
            Ok(stream.boxed())
        })
    }
}

impl OpenAiModel {
    fn pick_model(&self, request: &ChatRequest) -> String {
        if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        }
    }

    async fn send(&self, body: Value) -> Result<reqwest::Response, ProviderError> {
        self.client
            .http
            .post(format!("{}/v1/chat/completions", self.client.base_url))
            .bearer_auth(&self.client.api_key)
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

/// Builds the `OpenAI` (non-streaming) request body for a [`ChatRequest`].
///
/// Exposed for offline body/cache-shape tests; the request's `model` field is
/// used. Not part of the stable API.
#[doc(hidden)]
#[must_use]
pub fn build_request_body(req: &ChatRequest) -> Value {
    build_body(&req.model, req, false)
}

fn build_body(model: &str, req: &ChatRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_completion_tokens".into(), json!(req.max_tokens));
    body.insert(
        "messages".into(),
        json!(map_messages(req.system.as_ref(), &req.messages)),
    );
    if !req.tools.is_empty() {
        body.insert("tools".into(), json!(map_tools(&req.tools)));
        body.insert("tool_choice".into(), map_tool_choice(&req.tool_choice));
    }
    if let Some(temp) = req.temperature {
        body.insert("temperature".into(), json!(temp));
    }
    if !req.stop_sequences.is_empty() {
        body.insert("stop".into(), json!(req.stop_sequences));
    }
    // OpenAI auto-caches stable prefixes; the cache key only routes a
    // conversation to one cache shard (it does not enable caching itself).
    if let Some(key) = &req.cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if stream {
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    for (k, v) in &req.extra {
        body.insert(k.clone(), v.clone());
    }
    Value::Object(body)
}

fn map_messages(system: Option<&SystemPrompt>, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(sys) = system {
        out.push(json!({ "role": "system", "content": sys.text }));
    }
    for message in messages {
        match message {
            Message::User(parts) => map_user_parts(parts, &mut out),
            Message::Assistant(parts) => out.push(map_assistant(parts)),
        }
    }
    out
}

fn map_user_parts(parts: &[UserContent], out: &mut Vec<Value>) {
    // Tool results are independent `role: "tool"` messages.
    for p in parts {
        if let UserContent::ToolResult { id, content, .. } = p {
            out.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": tool_result_text(content),
            }));
        }
    }
    // Check if there are any images in this user turn.
    let has_images = parts.iter().any(|p| matches!(p, UserContent::Image(_)));
    if has_images {
        // Emit a multi-part content array combining text and images.
        let mut content_parts: Vec<Value> = Vec::new();
        for p in parts {
            match p {
                UserContent::Text(t) => {
                    content_parts.push(json!({ "type": "text", "text": &**t }));
                }
                UserContent::Image(img) => {
                    content_parts.push(map_image_block(img));
                }
                UserContent::ToolResult { .. } => {}
            }
        }
        if !content_parts.is_empty() {
            out.push(json!({ "role": "user", "content": content_parts }));
        }
    } else {
        let text: String = parts
            .iter()
            .filter_map(|p| match p {
                UserContent::Text(t) => Some(&**t),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            out.push(json!({ "role": "user", "content": text }));
        }
    }
}

/// Maps an [`Image`] to an `OpenAI` content part block.
fn map_image_block(img: &Image) -> Value {
    match img {
        Image::Url { url } => {
            json!({ "type": "image_url", "image_url": { "url": &**url } })
        }
        Image::Base64 { media_type, data } => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(data);
            let data_uri = format!("data:{};base64,{b64}", &**media_type);
            json!({ "type": "image_url", "image_url": { "url": data_uri } })
        }
        Image::FileId { file_id } => {
            // OpenAI file input uses the "input_image" type with file_id.
            // https://platform.openai.com/docs/guides/images?api-mode=responses
            json!({ "type": "input_image", "file_id": &**file_id })
        }
    }
}

fn tool_result_text(content: &[ToolResultPart]) -> String {
    content
        .iter()
        .map(|p| match p {
            ToolResultPart::Text(t) => t.clone(),
            ToolResultPart::Image(_) => "[image]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn map_assistant(parts: &[AssistantContent]) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for part in parts {
        match part {
            AssistantContent::Text(t) => text.push_str(t),
            AssistantContent::ToolUse { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": input.to_string() },
            })),
            AssistantContent::Thinking(_) => {}
        }
    }
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), json!(tool_calls));
    }
    Value::Object(msg)
}

fn map_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut function = Map::new();
            function.insert("name".into(), json!(t.name));
            function.insert("description".into(), json!(t.description));
            function.insert("parameters".into(), t.parameters.clone());
            if t.strict {
                function.insert("strict".into(), json!(true));
            }
            json!({ "type": "function", "function": Value::Object(function) })
        })
        .collect()
}

fn map_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Any | ToolChoice::Required => json!("required"),
        ToolChoice::None => json!("none"),
        ToolChoice::Tool(name) => json!({ "type": "function", "function": { "name": name } }),
    }
}

// --- response mapping ------------------------------------------------------

fn parse_response(value: &Value) -> ChatResponse {
    let choice = value.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));
    let mut content = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            content.push(AssistantContent::Text(text.into()));
        }
    }
    if let Some(calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            if let Some(block) = parse_tool_call(call) {
                content.push(block);
            }
        }
    }
    let stop = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map_or(StopReason::EndTurn, map_finish_reason);
    let usage = value.get("usage").map(map_usage).unwrap_or_default();
    ChatResponse {
        content,
        stop,
        usage,
    }
}

fn parse_tool_call(call: &Value) -> Option<AssistantContent> {
    let function = call.get("function")?;
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    Some(AssistantContent::ToolUse {
        id: call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        name: function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        input: serde_json::from_str(arguments).unwrap_or_else(|_| json!({})),
    })
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

fn map_usage(usage: &Value) -> Usage {
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("prompt_tokens"),
        output_tokens: n("completion_tokens"),
        cache_create_tokens: 0,
        cache_read_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

// --- streaming accumulator -------------------------------------------------

#[derive(Default)]
struct Accum {
    usage: Usage,
    tools: indexmap::IndexMap<u64, ToolBuf>,
    stop: Option<StopReason>,
}

#[derive(Default)]
struct ToolBuf {
    id: String,
    name: String,
    args: String,
}

impl Accum {
    fn push(&mut self, event: &Value) -> Vec<StreamEvent> {
        if let Some(usage) = event.get("usage").filter(|u| !u.is_null()) {
            self.usage = map_usage(usage);
        }
        let Some(choice) = event.get("choices").and_then(|c| c.get(0)) else {
            return Vec::new();
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(map_finish_reason(reason));
        }
        let Some(delta) = choice.get("delta") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                out.push(StreamEvent::TextDelta(text.to_owned()));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let buf = self.tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    id.clone_into(&mut buf.id);
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        buf.name.push_str(name);
                    }
                    if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                        buf.args.push_str(args);
                    }
                }
            }
        }
        out
    }

    /// Emits the accumulated tool calls and the terminal `End` event.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        for (_, buf) in std::mem::take(&mut self.tools) {
            let input = if buf.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&buf.args).unwrap_or_else(|_| json!({}))
            };
            out.push(StreamEvent::ToolCall {
                id: buf.id,
                name: buf.name,
                input,
            });
        }
        out.push(StreamEvent::End {
            stop: self.stop.take().unwrap_or(StopReason::EndTurn),
            usage: self.usage,
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_maps_system_and_tool_messages() {
        let mut req = ChatRequest::new("gpt-4o");
        req.system = Some(SystemPrompt::from("be brief"));
        req.messages = vec![
            Message::user_text("hi"),
            Message::User(vec![UserContent::ToolResult {
                id: "call_1".into(),
                content: vec![ToolResultPart::Text("42".into())],
                is_error: false,
            }]),
        ];
        let body = build_body("gpt-4o", &req, false);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], "42");
    }

    #[test]
    fn extra_passthrough_carries_reasoning_effort() {
        let mut req = ChatRequest::new("o4-mini");
        req.extra.insert("reasoning_effort".into(), json!("high"));
        let body = build_body("o4-mini", &req, false);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn parse_response_reads_tool_call_and_usage() {
        let value = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "wx", "arguments": "{\"city\":\"Paris\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 4,
                       "prompt_tokens_details": { "cached_tokens": 8 } }
        });
        let resp = parse_response(&value);
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.cache_read_tokens, 8);
        assert_eq!(
            resp.content[0],
            AssistantContent::ToolUse {
                id: "call_9".into(),
                name: "wx".into(),
                input: json!({ "city": "Paris" }),
            }
        );
    }

    #[test]
    fn accumulator_builds_tool_call_from_chunks() {
        let mut accum = Accum::default();
        accum.push(&json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": 0, "id": "call_1", "function": { "name": "wx", "arguments": "{\"c\":" } } ] } }] }));
        accum.push(&json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": 0, "function": { "arguments": "1}" } } ] }, "finish_reason": "tool_calls" }] }));
        accum.push(
            &json!({ "choices": [], "usage": { "prompt_tokens": 5, "completion_tokens": 2 } }),
        );
        let out = accum.finish();
        assert_eq!(
            out[0],
            StreamEvent::ToolCall {
                id: "call_1".into(),
                name: "wx".into(),
                input: json!({ "c": 1 }),
            }
        );
        let StreamEvent::End { stop, usage } = &out[1] else {
            panic!("expected End");
        };
        assert_eq!(*stop, StopReason::ToolUse);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn text_deltas_stream() {
        let mut accum = Accum::default();
        let out = accum.push(&json!({ "choices": [{ "delta": { "content": "Hi" } }] }));
        assert_eq!(out, vec![StreamEvent::TextDelta("Hi".into())]);
    }
}
