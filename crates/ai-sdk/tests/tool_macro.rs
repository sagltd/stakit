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
        .call_json(&cx, serde_json::json!({}))
        .await
        .expect("call");
    assert_eq!(out, serde_json::json!("pong"));
}

#[tool(description = "Read and increment the context counter.")]
async fn context_only(cx: &ToolCx<std::sync::atomic::AtomicUsize>) -> Result<usize, ToolError> {
    Ok(cx.ctx().fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
}

#[test]
fn tools_without_arguments_advertise_a_closed_empty_object() {
    let expected = serde_json::json!({
        "type": "object", "properties": {}, "required": [], "additionalProperties": false
    });
    assert_eq!(ToolDyn::<()>::def(&TypedTool(ping)).parameters, expected);
    assert_eq!(
        ToolDyn::<std::sync::atomic::AtomicUsize>::def(&TypedTool(context_only)).parameters,
        expected
    );
}

#[tokio::test]
async fn context_only_tool_accepts_empty_object_and_runs_once() {
    let cx = ToolCx::new(std::sync::atomic::AtomicUsize::new(0));
    let output = TypedTool(context_only)
        .call_json(&cx, serde_json::json!({}))
        .await
        .expect("call");
    assert_eq!(output, serde_json::json!(1));
    assert_eq!(cx.ctx().load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tools_without_arguments_reject_unexpected_input_before_effects() {
    let cx = ToolCx::new(std::sync::atomic::AtomicUsize::new(0));
    for input in [
        serde_json::json!({"unexpected": true}),
        serde_json::Value::Null,
        serde_json::json!([]),
        serde_json::json!(false),
        serde_json::json!(1),
        serde_json::json!("text"),
    ] {
        assert!(
            TypedTool(context_only)
                .call_json(&cx, input.clone())
                .await
                .is_err()
        );
        assert!(
            TypedTool(ping)
                .call_json(&ToolCx::new(()), input)
                .await
                .is_err()
        );
    }
    assert_eq!(cx.ctx().load(std::sync::atomic::Ordering::SeqCst), 0);
}
