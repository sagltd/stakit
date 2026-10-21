//! `OpenAI` provider (Chat Completions API).
//!
//! Maps the unified [`ChatRequest`] to `OpenAI`'s Chat Completions wire format and
//! back. Tool results become separate `role: "tool"` messages; assistant tool
//! calls live in a `tool_calls` array with stringified JSON arguments (parsed on
//! ingest). Streaming accumulates per-index `tool_calls` argument fragments and
//! emits a whole [`crate::StreamEvent::ToolCall`] when the call finishes; usage arrives
//! in a trailing chunk via `stream_options.include_usage`.

use serde_json::{Map, Value, json};

use crate::message::{AssistantContent, Document, Image, Message, ToolResultPart, UserContent};
use crate::provider::http::{
    HttpConfig, api_error, ensure_success, http_client_builders, pick_model, send, wire_count,
};
use crate::provider::{
    ChatRequest, ChatResponse, EventStream, Provider, StopReason, ToolChoice, ToolDef,
    parse_retry_after,
};
use crate::usage::Usage;
use crate::{CacheCapabilities, ProviderCapabilities, ProviderError, SystemPrompt};

mod accum;
mod capabilities;
mod response;
mod sse;
mod usage;
#[cfg(test)]
mod usage_presence_test;
use accum::Accum;
use response::parse_response;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
/// Max in-flight tool-call buffers a stream may open. A hostile stream could
/// otherwise allocate per-index entries without bound.
const MAX_INFLIGHT_TOOLS: usize = crate::provider::MAX_STREAM_OPEN_BLOCKS;
/// Max accumulated argument bytes for a single tool call. A hostile stream could
/// otherwise grow one tool's argument buffer past the per-line SSE cap.
#[cfg(test)]
const MAX_TOOL_ARGS: usize = crate::provider::MAX_STREAM_BLOCK_BYTES;

/// A handle to the `OpenAI` API. Cheap to clone; mints per-model handles.
#[derive(Clone)]
pub struct OpenAiClient {
    config: HttpConfig,
}

// Hand-written so the API key never leaks through `{:?}` (logs, panics, tests).
impl std::fmt::Debug for OpenAiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.config.debug("OpenAiClient", f)
    }
}

impl OpenAiClient {
    /// Builds a client for the given API key (default endpoint).
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            config: HttpConfig::new(api_key.into(), DEFAULT_BASE_URL),
        }
    }

    /// Reads the key from `OPENAI_API_KEY`.
    ///
    /// # Errors
    /// Returns [`ProviderError::InvalidArgument`] if the variable is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        HttpConfig::key_from_env("OPENAI_API_KEY").map(Self::new)
    }

    http_client_builders!(
        base_url_doc: "Overrides the base URL (e.g. an `OpenAI`-compatible endpoint).",
        path: "/v1/chat/completions"
    );

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

    fn capabilities(&self) -> ProviderCapabilities {
        capabilities::model_capabilities(&self.model)
    }

    fn capabilities_for(&self, model: &str) -> ProviderCapabilities {
        capabilities::model_capabilities(model)
    }

    fn cache_capabilities(&self) -> CacheCapabilities {
        self.cache_capabilities_for(&self.model)
    }

    fn cache_capabilities_for(&self, model: &str) -> CacheCapabilities {
        capabilities::cache_capabilities(model)
    }

    fn complete(
        &self,
        request: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move {
            let model = pick_model(&self.model, &request);
            request.validate_cache(&self.cache_capabilities_for(&model))?;
            let body = build_body(&model, &request, false);
            validate_body(&body)?;
            let resp = self.send(&body).await?;
            let status = resp.status();
            let retry_after = parse_retry_after(resp.headers());
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            if !status.is_success() {
                return Err(api_error(
                    status.as_u16(),
                    &String::from_utf8_lossy(&bytes),
                    retry_after,
                ));
            }
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|error| ProviderError::Decode {
                    err: format!(
                        "invalid OpenAI response JSON at line {} column {}",
                        error.line(),
                        error.column()
                    ),
                    body: String::new(),
                })?;
            parse_response(&value)
        })
    }

    fn stream(
        &self,
        request: ChatRequest,
    ) -> futures::future::BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            let model = pick_model(&self.model, &request);
            request.validate_cache(&self.cache_capabilities_for(&model))?;
            let body = build_body(&model, &request, true);
            validate_body(&body)?;
            let resp = ensure_success(self.send(&body).await?).await?;
            Ok(sse::decode(resp.bytes_stream()))
        })
    }
}

impl OpenAiModel {
    async fn send(&self, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let request = self.client.config.post("/v1/chat/completions", body);
        send(request.map(|request| request.bearer_auth(&self.client.config.api_key))).await
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

/// Validate the effective body after extras: extras must not bypass known-model
/// restrictions, including overriding the generated reasoning mode with null.
fn validate_body(body: &Value) -> Result<(), ProviderError> {
    let known = body["model"]
        .as_str()
        .is_some_and(capabilities::is_registered_chat_model);
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if known && tools && body["reasoning_effort"].as_str() != Some("none") {
        return Err(ProviderError::InvalidArgument(
            "this Chat Completions model requires reasoning_effort=none when using tools".into(),
        ));
    }
    Ok(())
}

fn build_body(model: &str, req: &ChatRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    let capabilities = capabilities::model_capabilities(model);
    let max_tokens = capabilities
        .max_output_tokens
        .map_or(req.max_tokens, |limit| req.max_tokens.min(limit));
    body.insert("max_completion_tokens".into(), json!(max_tokens));
    if capabilities::is_registered_chat_model(model) {
        // These models allow Chat Completions tools only without reasoning.
        // Reasoning with tools requires a separate Responses API adapter.
        body.insert("reasoning_effort".into(), json!("none"));
    }
    body.insert(
        "messages".into(),
        json!(map_messages(req.system.as_ref(), &req.messages)),
    );
    if !req.tools.is_empty() {
        body.insert("tools".into(), json!(map_tools(&req.tools)));
        body.insert("tool_choice".into(), map_tool_choice(&req.tool_choice));
        if let Some(parallel) = req.parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }
    if let Some(temp) = req.temperature.filter(|_| capabilities.sampling_params) {
        body.insert("temperature".into(), json!(temp));
    }
    if !req.stop_sequences.is_empty() {
        body.insert("stop".into(), json!(req.stop_sequences));
    }
    // Caching is provider-managed. A stable opaque key scopes cache accounting;
    // on earlier models it also influences routing, without guaranteeing a hit.
    if let Some(key) = &req.cache_key {
        body.insert("prompt_cache_key".into(), json!(&**key));
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
    // Images and documents need a multi-part content array.
    let multipart = parts
        .iter()
        .any(|p| matches!(p, UserContent::Image(_) | UserContent::Document(_)));
    if multipart {
        // Emit a multi-part content array combining text, images and files.
        let mut content_parts: Vec<Value> = Vec::new();
        for p in parts {
            match p {
                UserContent::Text(t) => {
                    content_parts.push(json!({ "type": "text", "text": &**t }));
                }
                UserContent::Image(img) => {
                    content_parts.push(map_image_block(img));
                }
                UserContent::Document(doc) => content_parts.push(map_document_part(doc)),
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

/// Maps a [`Document`] to an `OpenAI` `file` content part. Chat Completions
/// has no URL form, so a URL document degrades to its address as text.
fn map_document_part(doc: &Document) -> Value {
    match doc {
        Document::Url { url } => json!({ "type": "text", "text": &**url }),
        Document::FileId { file_id } => {
            json!({ "type": "file", "file": { "file_id": &**file_id } })
        }
        Document::Base64 { media_type, data } => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(data);
            let filename = if &**media_type == "application/pdf" {
                "document.pdf"
            } else {
                "document"
            };
            json!({
                "type": "file",
                "file": {
                    "filename": filename,
                    "file_data": format!("data:{};base64,{b64}", &**media_type),
                },
            })
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
            // note: base64 is re-encoded on every step when the same inline image
            // is replayed. Caching it would need an `OnceLock` inside
            // `Image::Base64`, which breaks the enum's `PartialEq`/`Eq`/`Serialize`
            // derives; prefer `Image::Url`/`Image::FileId` for repeated images.
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
            ToolResultPart::Text(t) => &**t,
            ToolResultPart::Image(_) => "[image]",
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
    let n = |key: &str| wire_count(usage, key);
    let details = &usage["prompt_tokens_details"];
    let cache_read_tokens = details["cached_tokens"].as_u64().unwrap_or(0);
    let cache_create_tokens = details["cache_write_tokens"].as_u64().unwrap_or(0);
    Usage {
        // OpenAI prompt_tokens includes cache reads and writes. The shared
        // contract stores disjoint buckets so Pricing never bills them twice.
        input_tokens: n("prompt_tokens")
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_create_tokens),
        output_tokens: n("completion_tokens"),
        cache_create_tokens,
        cache_create_1h_tokens: 0,
        cache_read_tokens,
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamEvent;
    use crate::provider::test_util::{assert_open_tools_capped, expect_decode_error};

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
        let resp = parse_response(&value).expect("valid response");
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 4);
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
            "index": 0, "id": "call_1", "function": { "name": "wx", "arguments": "{\"c\":" } } ] } }] })).expect("push ok");
        accum.push(&json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": 0, "function": { "arguments": "1}" } } ] }, "finish_reason": "tool_calls" }] })).expect("push ok");
        accum
            .push(
                &json!({ "choices": [], "usage": { "prompt_tokens": 5, "completion_tokens": 2 } }),
            )
            .expect("push ok");
        let out = accum.finish().expect("valid terminal");
        assert_eq!(
            out[0],
            StreamEvent::ToolCall {
                id: "call_1".into(),
                name: "wx".into(),
                arguments: "{\"c\":1}".to_owned(),
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
        let out = accum
            .push(&json!({ "choices": [{ "delta": { "content": "Hi" } }] }))
            .expect("push ok");
        assert_eq!(out, vec![StreamEvent::TextDelta("Hi".into())]);
    }

    #[test]
    fn accumulator_caps_inflight_tool_count() {
        let mut accum = Accum::default();
        assert_open_tools_capped(|index| {
            accum.push(&json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": index, "id": format!("c{index}"),
                "function": { "name": "wx", "arguments": "" } }] } }] }))
        });
    }

    #[test]
    fn accumulator_caps_single_tool_arg_buffer() {
        let mut accum = Accum::default();
        let huge = "x".repeat(MAX_TOOL_ARGS + 1);
        expect_decode_error(
            accum.push(&json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": 0, "id": "c0", "function": { "name": "wx", "arguments": huge } }] } }] })),
        );
    }
}

#[cfg(test)]
#[path = "openai/usage_test.rs"]
mod usage_test;

#[cfg(test)]
#[path = "openai/model_test.rs"]
mod model_test;
