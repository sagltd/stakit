//! Tests that `cx.set_model()` re-resolves the provider mid-run.
//!
//! A `Switcher` middleware fires `cx.set_model("b")` in `on_step` once
//! `cx.index() >= 1`. Each `TaggedProvider` tracks its own call count so we
//! can assert that provider "b"'s `stream` was actually invoked.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, ChatRequest, ChatResponse, EventStream, Finish,
    Flow, Message, Provider, ProviderError, StopReason, StreamEvent, Tool, ToolCx, ToolError,
    Usage, event_stream,
};
use stakit_model::{JsonSchema, Model};

// ── A provider that stamps its id in the text it emits ───────────────────────

#[derive(Clone)]
struct TaggedProvider {
    id: &'static str,
    calls: Arc<AtomicU32>,
    /// How this provider should respond: emit a tool call (`true`) or text (`false`).
    tool_call: bool,
}

impl TaggedProvider {
    fn new(id: &'static str, tool_call: bool) -> Self {
        Self {
            id,
            calls: Arc::new(AtomicU32::new(0)),
            tool_call,
        }
    }

    fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for TaggedProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        self.id
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move { Err(ProviderError::InvalidArgument("unused".into())) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let id = self.id;
        let tool_call = self.tool_call;

        Box::pin(async move {
            let events: Vec<Result<StreamEvent, ProviderError>> = if tool_call {
                // Step 0: emit a tool call so the loop continues to a second step.
                vec![
                    Ok(StreamEvent::Start {
                        usage: Usage {
                            input_tokens: 5,
                            ..Usage::default()
                        },
                    }),
                    Ok(StreamEvent::ToolCall {
                        id: "noop1".into(),
                        name: "noop".into(),
                        input: serde_json::json!({}),
                    }),
                    Ok(StreamEvent::End {
                        stop: StopReason::ToolUse,
                        usage: Usage {
                            input_tokens: 5,
                            output_tokens: 2,
                            ..Usage::default()
                        },
                    }),
                ]
            } else {
                // Final step: emit tagged text and end.
                vec![
                    Ok(StreamEvent::TextDelta(format!("from-{id}"))),
                    Ok(StreamEvent::End {
                        stop: StopReason::EndTurn,
                        usage: Usage {
                            input_tokens: 5,
                            output_tokens: 3,
                            ..Usage::default()
                        },
                    }),
                ]
            };
            Ok(event_stream(events))
        })
    }
}

// ── A no-op tool (so the tool call in step 0 resolves) ───────────────────────

#[derive(serde::Deserialize, Model, JsonSchema)]
struct NoopArgs {}

struct NoopTool;

impl Tool<()> for NoopTool {
    type Args = NoopArgs;
    type Output = String;

    fn name(&self) -> &'static str {
        "noop"
    }
    fn description(&self) -> &'static str {
        "Does nothing"
    }
    fn run<'a>(
        &'a self,
        _cx: &'a ToolCx<()>,
        _args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        Box::pin(async move { Ok("ok".into()) })
    }
}

// ── Middleware: switch to "b" before step 1 ──────────────────────────────────

struct Switcher;

#[async_trait::async_trait]
impl AgentMiddleware<()> for Switcher {
    async fn on_step(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        if cx.index() >= 1 {
            cx.set_model("b");
        }
        Ok(Flow::Continue)
    }
}

// ── The test ─────────────────────────────────────────────────────────────────

/// Provider "a" drives step 0 (emits a tool call); the `Switcher` middleware
/// sets model "b" before step 1; provider "b" emits the final text. We verify:
/// 1. `out.text` contains "from-b" (provider "b" produced the answer).
/// 2. provider "b" 's `stream` was called at least once.
/// 3. The run ended with `Finish::EndTurn`.
#[tokio::test]
async fn set_model_switches_provider_mid_run() {
    let provider_a = TaggedProvider::new("a", /* tool_call */ true);
    let provider_b = TaggedProvider::new("b", /* tool_call */ false);

    let calls_b = Arc::clone(&provider_b.calls);

    let mut agent = Agent::new(())
        .provider(provider_a.clone())
        .register_provider(provider_b)
        .model("a")
        .register_tool(NoopTool)
        .register_middleware(Switcher)
        .with_context(vec![Message::user("go")]);

    let out = agent.run().await.expect("outcome");

    // The final text must come from provider "b".
    assert!(
        out.text.contains("from-b"),
        "expected text from provider b, got {:?}",
        out.text
    );

    // Provider "b"'s stream was invoked.
    assert!(
        calls_b.load(Ordering::SeqCst) > 0,
        "provider 'b' was never called — set_model did not re-resolve the provider"
    );

    // Provider "a" handled exactly step 0.
    assert_eq!(
        provider_a.call_count(),
        1,
        "provider 'a' should have been called exactly once"
    );

    assert!(
        matches!(out.finish, Finish::EndTurn),
        "expected EndTurn, got {:?}",
        out.finish
    );
}

/// Registering a second provider does not change the active model.
#[tokio::test]
async fn register_provider_does_not_steal_default_model() {
    let agent = Agent::new(())
        .provider(TaggedProvider::new("a", false))
        .register_provider(TaggedProvider::new("b", false));
    assert_eq!(agent.current_model(), "a");
}

/// After `set_model("b")` via middleware, a run that never hits a tool call
/// should still come from provider "b" when the switch fires on step 0.
#[tokio::test]
async fn set_model_in_on_start_uses_new_provider_immediately() {
    struct SwitchOnStart;

    #[async_trait::async_trait]
    impl AgentMiddleware<()> for SwitchOnStart {
        async fn on_start(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
            cx.set_model("b");
            Ok(Flow::Continue)
        }
    }

    let provider_b = TaggedProvider::new("b", false);
    let calls_b = Arc::clone(&provider_b.calls);

    let mut agent = Agent::new(())
        .provider(TaggedProvider::new("a", false))
        .register_provider(provider_b)
        .model("a")
        .register_middleware(SwitchOnStart)
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert!(
        out.text.contains("from-b"),
        "expected text from b, got {:?}",
        out.text
    );
    assert!(
        calls_b.load(Ordering::SeqCst) > 0,
        "provider b was never called"
    );
    assert!(matches!(out.finish, Finish::EndTurn));
}

/// Switching to a model that has no registered provider stops with an error.
#[tokio::test]
async fn set_model_to_unregistered_stops_with_error() {
    struct SwitchToGhost;

    #[async_trait::async_trait]
    impl AgentMiddleware<()> for SwitchToGhost {
        async fn on_step(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
            cx.set_model("ghost");
            Ok(Flow::Continue)
        }
    }

    let mut agent = Agent::new(())
        .provider(TaggedProvider::new("a", false))
        .register_middleware(SwitchToGhost)
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Stopped, got {:?}",
        out.finish
    );
    let Finish::Stopped { message } = out.finish else {
        unreachable!()
    };
    assert!(
        message.unwrap_or_default().contains("ghost"),
        "error should mention the missing model id"
    );
}
