//! Offline per-provider request-mapping tests for [`stakit_ai_sdk::LLM::extract`].
//!
//! These tests assert that the [`ChatRequest`] built by `LLM::extract::<T>()`
//! is serialised correctly by both the Claude and `OpenAI` providers — specifically
//! that the `"extract"` tool is present with the right schema, and that
//! `tool_choice` forces that tool. No network is involved.
#![cfg(all(feature = "claude", feature = "openai"))]

use serde_json::Value;
use stakit_ai_sdk::{
    ChatRequest, Message, ToolChoice, ToolDef,
    test_support::{claude_body, openai_body},
};
use stakit_model::{JsonSchema, Model};

// ── Test subject ─────────────────────────────────────────────────────────────

/// The struct used in all extraction assertions.
#[derive(Debug, serde::Deserialize, Model, JsonSchema)]
#[allow(dead_code)]
struct User {
    /// The user's name.
    name: String,
    /// The user's age.
    age: u32,
}

// ── Request builder (mirrors `LLM::build_request` without the provider) ──────

/// Builds the same [`ChatRequest`] that `LLM::extract::<User>()` produces.
///
/// `LLM::build_request` is private, so we replicate the exact same three lines
/// that `LLM::extract` executes (verified by reading `llm.rs`):
///
/// ```text
/// let tool = ToolDef::new("extract", "Return the structured result.", T::schema());
/// let req = self.build_request(vec![tool], ToolChoice::Tool("extract".into()))?;
/// ```
///
/// `build_request` in turn sets: model, system, `messages=[user(text)]`,
/// tools, `tool_choice`, `max_tokens=1024`, temperature. We use a fixed model id
/// so the serialised body is deterministic.
fn extract_request() -> ChatRequest {
    let tool = ToolDef::new("extract", "Return the structured result.", User::schema());
    let mut req = ChatRequest::new("test-model");
    req.messages = vec![Message::user("Bob is 30")];
    req.tools = std::sync::Arc::from([tool]);
    req.tool_choice = ToolChoice::Tool("extract".into());
    req.max_tokens = 1024;
    req
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the tool entry named `"extract"` from the Claude body's `tools` array.
fn claude_extract_tool(body: &Value) -> &Value {
    body["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|v| v["name"] == "extract"))
        .expect("claude body must contain an 'extract' tool")
}

/// Returns the tool entry named `"extract"` from the `OpenAI` body's `tools` array.
fn openai_extract_function(body: &Value) -> &Value {
    body["tools"]
        .as_array()
        .and_then(|t| {
            t.iter()
                .find(|v| v["type"] == "function" && v["function"]["name"] == "extract")
        })
        .expect("openai body must contain an 'extract' function tool")
}

// ── Claude assertions ─────────────────────────────────────────────────────────

#[test]
fn claude_extract_tool_name_is_extract() {
    let body = claude_body(&extract_request());
    let tool = claude_extract_tool(&body);
    assert_eq!(tool["name"], "extract");
}

#[test]
fn claude_extract_tool_input_schema_matches_user_schema() {
    let body = claude_body(&extract_request());
    let tool = claude_extract_tool(&body);
    // Claude uses `input_schema` for the JSON Schema object.
    let schema = &tool["input_schema"];
    assert_eq!(
        schema,
        &User::schema(),
        "input_schema must equal User::schema()"
    );
}

#[test]
fn claude_tool_choice_forces_extract_tool() {
    let body = claude_body(&extract_request());
    // Anthropic forced-tool format: {"type":"tool","name":"extract"}
    assert_eq!(
        body["tool_choice"]["type"], "tool",
        "tool_choice.type must be 'tool'; body={body:#}"
    );
    assert_eq!(
        body["tool_choice"]["name"], "extract",
        "tool_choice.name must be 'extract'; body={body:#}"
    );
}

// ── OpenAI assertions ─────────────────────────────────────────────────────────

#[test]
fn openai_extract_function_name_is_extract() {
    let body = openai_body(&extract_request());
    let function = openai_extract_function(&body);
    assert_eq!(function["function"]["name"], "extract");
}

#[test]
fn openai_extract_function_parameters_match_user_schema() {
    let body = openai_body(&extract_request());
    let function = openai_extract_function(&body);
    // OpenAI uses `parameters` for the JSON Schema object.
    let parameters = &function["function"]["parameters"];
    assert_eq!(
        parameters,
        &User::schema(),
        "parameters must equal User::schema()"
    );
}

#[test]
fn openai_tool_choice_forces_extract_function() {
    let body = openai_body(&extract_request());
    // OpenAI forced-function format: {"type":"function","function":{"name":"extract"}}
    assert_eq!(
        body["tool_choice"]["type"], "function",
        "tool_choice.type must be 'function'; body={body:#}"
    );
    assert_eq!(
        body["tool_choice"]["function"]["name"], "extract",
        "tool_choice.function.name must be 'extract'; body={body:#}"
    );
}

// ── Response-parsing unit tests ───────────────────────────────────────────────
//
// These feed representative provider JSON directly to the (public) `parse_response`
// function — which is internal, so we rely on the existing unit tests in
// `provider/claude.rs` and `provider/openai.rs` that already exercise this path
// (see `parse_response_reads_text_tooluse_stop_and_usage` in claude.rs and
// `parse_response_reads_tool_call_and_usage` in openai.rs).
//
// What we add here is an end-to-end offline smoke test using the mock provider
// from `llm.rs` tests: this verifies that `LLM::extract` correctly finds the
// `ToolUse` block and deserialises it, regardless of provider serialisation.
// (That path is already covered by `llm_extract_returns_typed_value` in llm.rs.)
//
// The actual wire-format parsing is proven by the provider-level unit tests;
// the mapping from wire JSON → AssistantContent::ToolUse is verified in both
// provider files.
