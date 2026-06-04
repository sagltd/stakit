//! Third-party extensibility proof: build a stateful agent and add a brand-new
//! provider, a hand-written tool, and a middleware using ONLY items re-exported
//! from `stakit_ai_sdk`.
//!
//! Nothing here touches crate internals — every type is reached through the
//! public `stakit_ai_sdk::*` surface. If this file compiles and passes, an
//! outside developer can ship their own provider, tools and middleware as a
//! downstream crate without forking or patching `stakit-ai-sdk`.
#![allow(dead_code)]

use futures::StreamExt;
use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentEvent, AgentMiddleware, Approval, AssistantContent,
    ChatRequest, ChatResponse, EventStream, Finish, Flow, Message, PendingToolCall, Provider,
    ProviderError, StopReason, StreamEvent, Tool, ToolCx, ToolDef, ToolDyn, ToolError, Usage,
    UserContent, tool,
};
use stakit_model::{JsonSchema, Model};

// ---------------------------------------------------------------------------
// (1) A brand-new provider implemented entirely against the public trait.
// ---------------------------------------------------------------------------

/// A fake provider that echoes the last user text back as the assistant reply.
#[derive(Clone)]
struct EchoProvider {
    model: String,
}

impl EchoProvider {
    fn new() -> Self {
        Self {
            model: "echo-1".to_string(),
        }
    }

    /// Pull the most recent user text out of the request history.
    fn last_user_text(request: &ChatRequest) -> String {
        request
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(parts) => parts.iter().find_map(|p| match p {
                    UserContent::Text(t) => Some(t.to_string()),
                    _ => None,
                }),
                Message::Assistant(_) => None,
            })
            .unwrap_or_default()
    }

    fn canned_usage() -> Usage {
        Usage {
            input_tokens: 7,
            output_tokens: 3,
            ..Usage::default()
        }
    }
}

impl Provider for EchoProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn complete(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move {
            let echoed = format!("echo: {}", Self::last_user_text(&request));
            Ok(ChatResponse {
                content: vec![AssistantContent::Text(echoed.into())],
                stop: StopReason::EndTurn,
                usage: Self::canned_usage(),
            })
        })
    }

    fn stream(&self, request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            let echoed = format!("echo: {}", Self::last_user_text(&request));
            let events: Vec<Result<StreamEvent, ProviderError>> = vec![
                Ok(StreamEvent::Start {
                    usage: Usage {
                        input_tokens: 7,
                        ..Usage::default()
                    },
                }),
                Ok(StreamEvent::TextDelta(echoed)),
                Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Self::canned_usage(),
                }),
            ];
            Ok(futures::stream::iter(events).boxed())
        })
    }
}

// ---------------------------------------------------------------------------
// (2) A tool registered via the `#[tool]` macro.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Model, JsonSchema)]
struct ShoutArgs {
    /// Text to shout
    text: String,
}

/// Shout the given text (uppercase it)
#[tool]
async fn shout(args: ShoutArgs) -> Result<String, ToolError> {
    Ok(args.text.to_uppercase())
}

// ---------------------------------------------------------------------------
// (3) A tool implemented BY HAND against the public `Tool` trait (no macro).
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Model, JsonSchema)]
struct ReverseArgs {
    /// Text to reverse
    text: String,
}

/// Hand-written tool: reverses its input. Proves the `Tool` trait is directly
/// implementable by an outsider without the attribute macro.
struct ReverseTool;

impl Tool<()> for ReverseTool {
    type Args = ReverseArgs;
    type Output = String;

    fn name(&self) -> &'static str {
        "reverse"
    }

    fn description(&self) -> &'static str {
        "Reverse the given text"
    }

    fn run<'a>(
        &'a self,
        _cx: &'a ToolCx<()>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        Box::pin(async move { Ok(args.text.chars().rev().collect()) })
    }
}

// ---------------------------------------------------------------------------
// (4) A middleware implemented BY HAND against the public trait.
// ---------------------------------------------------------------------------

/// Approves every tool call except those named in `block`.
struct Policy {
    block: &'static str,
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for Policy {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, ()>,
        call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        if call.name == self.block {
            Ok(Approval::Deny {
                message: format!("{} blocked by policy", call.name),
            })
        } else {
            Ok(Approval::Allow)
        }
    }

    async fn on_step(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        Ok(Flow::Continue)
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extensibility_outsider_builds_agent_over_custom_provider() {
    let mut agent = Agent::new(())
        .provider(EchoProvider::new())
        .register_tool(shout)
        .register_tool(ReverseTool)
        .register_middleware(Policy { block: "nothing" })
        .with_context(vec![Message::user("hello world")]);

    let mut run = agent.run();
    let mut saw_delta = false;
    let mut outcome = None;
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::MessageDelta(t) if t.contains("hello world") => saw_delta = true,
            AgentEvent::Done(o) => outcome = Some(o),
            _ => {}
        }
    }
    assert!(saw_delta, "expected a MessageDelta echoing the input");

    let out = outcome.expect("a Done outcome");
    assert!(matches!(out.finish, Finish::EndTurn));
    assert_eq!(out.text, "echo: hello world");
}

#[tokio::test]
async fn custom_provider_complete_works() {
    let provider = EchoProvider::new();
    let resp = provider
        .complete({
            let mut r = ChatRequest::new("echo-1");
            r.messages.push(Message::user("ping"));
            r
        })
        .await
        .expect("complete");
    assert_eq!(resp.stop, StopReason::EndTurn);
    assert_eq!(
        resp.content,
        vec![AssistantContent::Text("echo: ping".into())]
    );
}

#[tokio::test]
async fn hand_written_tool_runs_through_public_trait() {
    // Erase + invoke via the public ToolDyn/TypedTool surface (no macro).
    let def = ToolDyn::<()>::def(&stakit_ai_sdk::TypedTool(ReverseTool));
    assert_eq!(def.name, "reverse");
    assert_eq!(def.parameters["type"], "object");

    let cx = ToolCx::new(());
    let out = stakit_ai_sdk::TypedTool(ReverseTool)
        .call_json(&cx, serde_json::json!({ "text": "abc" }))
        .await
        .expect("call");
    assert_eq!(out, serde_json::json!("cba"));
}

#[tokio::test]
async fn custom_tooldef_is_constructible() {
    // Outsiders can also build a raw ToolDef directly (e.g. for a custom
    // ToolDyn impl that owns its own schema).
    let def = ToolDef::new(
        "custom",
        "a custom tool",
        serde_json::json!({ "type": "object" }),
    );
    assert!(!def.strict);
    assert!(!def.cache);
}
