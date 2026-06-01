//! End-to-end tests for the `#[tool]` attribute macro across signature shapes.
#![allow(dead_code)]

use stakit_ai_sdk::{ToolCx, ToolDyn, TypedTool, tool};
use stakit_model::{JsonSchema, Model};

#[derive(serde::Deserialize, Model, JsonSchema)]
struct AddArgs {
    a: i64,
    b: i64,
}

/// Add two integers
#[tool]
async fn add(args: AddArgs) -> Result<i64, ToolError> {
    Ok(args.a + args.b)
}

struct Db {
    prefix: String,
}

#[derive(serde::Deserialize, Model, JsonSchema)]
struct EchoArgs {
    /// Text to echo
    text: String,
}

#[tool]
async fn echo(cx: &ToolCx<Db>, args: EchoArgs) -> Result<String, ToolError> {
    Ok(format!("{}{}", cx.ctx().prefix, args.text))
}

#[tool(name = "pong", description = "returns pong")]
fn ping() -> Result<&'static str, ToolError> {
    Ok("pong")
}

#[tokio::test]
async fn args_only_tool_uses_fn_name_and_doc() {
    let def = ToolDyn::<()>::def(&TypedTool(add));
    assert_eq!(def.name, "add");
    assert_eq!(def.description, "Add two integers");
    let cx = ToolCx::new(());
    let out = TypedTool(add)
        .call_json(&cx, serde_json::json!({ "a": 2, "b": 3 }))
        .await
        .expect("call");
    assert_eq!(out, serde_json::json!(5));
}

#[tokio::test]
async fn cx_tool_reads_context() {
    let cx = ToolCx::new(Db {
        prefix: ">> ".into(),
    });
    let out = TypedTool(echo)
        .call_json(&cx, serde_json::json!({ "text": "hi" }))
        .await
        .expect("call");
    assert_eq!(out, serde_json::json!(">> hi"));
}

#[tokio::test]
async fn no_arg_tool_with_overridden_name_and_description() {
    let def = ToolDyn::<()>::def(&TypedTool(ping));
    assert_eq!(def.name, "pong");
    assert_eq!(def.description, "returns pong");
    let cx = ToolCx::new(());
    let out = TypedTool(ping)
        .call_json(&cx, serde_json::Value::Null)
        .await
        .expect("call");
    assert_eq!(out, serde_json::json!("pong"));
}
