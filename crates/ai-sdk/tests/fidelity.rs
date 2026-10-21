//! Offline request-body tests for the cross-dialect fidelity fields: documents
//! and the parallel-tool-calls switch, on both built-in providers.
#![cfg(all(feature = "claude", feature = "openai"))]

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use stakit_ai_sdk::test_support::{claude_body, openai_body};
use stakit_ai_sdk::{ChatRequest, Document, Message, ToolChoice, ToolDef, UserContent};

const CLAUDE: &str = "claude-sonnet-5-5";

fn with_tool(model: &str, choice: ToolChoice, parallel: Option<bool>) -> ChatRequest {
    let mut request = ChatRequest::new(model);
    request.messages = vec![Message::user("hi")];
    request.tools = Arc::from([ToolDef::new("lookup", "Look.", json!({ "type": "object" }))]);
    request.tool_choice = choice;
    request.parallel_tool_calls = parallel;
    request
}

fn with_document(model: &str, document: Document) -> ChatRequest {
    let mut request = ChatRequest::new(model);
    request.messages = vec![Message::User(vec![
        UserContent::Document(document),
        UserContent::Text("summarize".into()),
    ])];
    request
}

fn pdf() -> Document {
    Document::Base64 {
        media_type: "application/pdf".into(),
        data: Bytes::from_static(b"%PDF-1.7"),
    }
}

#[test]
fn claude_disables_parallel_tool_use_only_when_asked() {
    let off = claude_body(&with_tool(CLAUDE, ToolChoice::Auto, Some(false)));
    let default = claude_body(&with_tool(CLAUDE, ToolChoice::Auto, None));
    let on = claude_body(&with_tool(CLAUDE, ToolChoice::Auto, Some(true)));

    assert_eq!(off["tool_choice"]["disable_parallel_tool_use"], true);
    assert_eq!(default["tool_choice"], json!({ "type": "auto" }));
    assert_eq!(on["tool_choice"], json!({ "type": "auto" }));
}

#[test]
fn claude_parallel_switch_applies_to_forced_tools_but_not_to_none() {
    let forced = claude_body(&with_tool(
        CLAUDE,
        ToolChoice::Tool("lookup".into()),
        Some(false),
    ));
    let any = claude_body(&with_tool(CLAUDE, ToolChoice::Any, Some(false)));
    let none = claude_body(&with_tool(CLAUDE, ToolChoice::None, Some(false)));

    assert_eq!(forced["tool_choice"]["disable_parallel_tool_use"], true);
    assert_eq!(any["tool_choice"]["disable_parallel_tool_use"], true);
    assert_eq!(none["tool_choice"], json!({ "type": "none" }));
}

#[test]
fn openai_sends_parallel_tool_calls_only_with_tools() {
    let off = openai_body(&with_tool("gpt-x", ToolChoice::Auto, Some(false)));
    let default = openai_body(&with_tool("gpt-x", ToolChoice::Auto, None));
    let mut no_tools = ChatRequest::new("gpt-x");
    no_tools.parallel_tool_calls = Some(false);

    assert_eq!(off["parallel_tool_calls"], false);
    assert!(default.get("parallel_tool_calls").is_none());
    assert!(openai_body(&no_tools).get("parallel_tool_calls").is_none());
}

#[test]
fn claude_maps_documents_to_document_blocks() {
    let base64 = claude_body(&with_document(CLAUDE, pdf()));
    let url = claude_body(&with_document(
        CLAUDE,
        Document::Url {
            url: "https://x/y.pdf".into(),
        },
    ));
    let file = claude_body(&with_document(
        CLAUDE,
        Document::FileId {
            file_id: "file_1".into(),
        },
    ));

    let first = &base64["messages"][0]["content"][0];
    assert_eq!(first["type"], "document");
    assert_eq!(first["source"]["type"], "base64");
    assert_eq!(first["source"]["media_type"], "application/pdf");
    assert_eq!(first["source"]["data"], "JVBERi0xLjc=");
    assert_eq!(
        url["messages"][0]["content"][0]["source"],
        json!({ "type": "url", "url": "https://x/y.pdf" })
    );
    assert_eq!(
        file["messages"][0]["content"][0]["source"],
        json!({ "type": "file", "file_id": "file_1" })
    );
}

#[test]
fn openai_maps_documents_to_file_parts() {
    let base64 = openai_body(&with_document("gpt-x", pdf()));
    let file = openai_body(&with_document(
        "gpt-x",
        Document::FileId {
            file_id: "file_1".into(),
        },
    ));

    let parts: &Value = &base64["messages"][0]["content"];
    assert_eq!(parts[0]["type"], "file");
    assert_eq!(parts[0]["file"]["filename"], "document.pdf");
    assert_eq!(
        parts[0]["file"]["file_data"],
        "data:application/pdf;base64,JVBERi0xLjc="
    );
    assert_eq!(parts[1], json!({ "type": "text", "text": "summarize" }));
    assert_eq!(
        file["messages"][0]["content"][0]["file"],
        json!({ "file_id": "file_1" })
    );
}
