//! Third-party extensibility proof: build an agent and add a brand-new provider
//! and a hand-written tool using ONLY items re-exported from `stakit_ai_sdk`.
//!
//! Nothing here touches crate internals — every type is reached through the
//! public `stakit_ai_sdk::*` surface. If this file compiles and passes, an
//! outside developer can ship their own provider and tools as a downstream
//! crate without forking or patching `stakit-ai-sdk`.
#![allow(dead_code)]
// The fake provider satisfies the async `Provider` trait but does no async work.
#![allow(clippy::unused_async_trait_impl)]

use futures::StreamExt;
use stakit_ai_sdk::{
    Agent, AssistantContent, BoxFuture, CancelToken, ChatRequest, ChatResponse, EventStream,
    FinishReason, LoopEvent, Message, Provider, ProviderError, StopReason, StreamEvent, Tool,
    ToolCx, ToolDef, ToolDyn, ToolError, Usage, UserContent, tool,
};
use stakit_model::{JsonSchema, Model};

// ---------------------------------------------------------------------------
// (1) A brand-new provider implemented entirely against the public trait.
// ---------------------------------------------------------------------------

/// A fake provider that echoes the last user text back as the assistant reply.
///
/// First turn: emit a single `TextDelta` echoing the last user message, then
/// `End { EndTurn }`. This is enough to drive the agent loop to completion.
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
                    UserContent::Text(t) => Some(t.clone()),
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
    type Raw = ();

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        request: ChatRequest,
    ) -> Result<ChatResponse<Self::Raw>, ProviderError> {
        let echoed = format!("echo: {}", Self::last_user_text(&request));
        Ok(ChatResponse {
            content: vec![AssistantContent::Text(echoed)],
            stop: StopReason::EndTurn,
            usage: Self::canned_usage(),
            raw: (),
        })
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream, ProviderError> {
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
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extensibility_outsider_builds_agent_over_custom_provider() {
    let agent = Agent::<EchoProvider, ()>::builder(EchoProvider::new())
        .register(shout)
        .register(ReverseTool)
        .build();

    assert_eq!(
        agent.tool_names(),
        vec!["shout".to_string(), "reverse".to_string()]
    );

    let events: Vec<LoopEvent> = agent
        .run(
            vec![Message::user_text("hello world")],
            (),
            CancelToken::new(),
        )
        .collect()
        .await;

    // The loop must have streamed at least one text delta with our echo.
    let saw_delta = events
        .iter()
        .any(|e| matches!(e, LoopEvent::TextDelta(t) if t.contains("hello world")));
    assert!(
        saw_delta,
        "expected a TextDelta echoing the input: {events:?}"
    );

    // And it must terminate with a clean Done.
    let Some(LoopEvent::Done { text, reason, .. }) = events.last() else {
        panic!("last event is not Done: {events:?}");
    };
    assert_eq!(*reason, FinishReason::EndTurn);
    assert_eq!(text, "echo: hello world");
}

#[tokio::test]
async fn custom_provider_complete_works() {
    let provider = EchoProvider::new();
    let resp = provider
        .complete({
            let mut r = ChatRequest::new("echo-1");
            r.messages.push(Message::user_text("ping"));
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
