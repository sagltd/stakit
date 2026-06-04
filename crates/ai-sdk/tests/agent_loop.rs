//! End-to-end tests for the stateful agent loop, driven by mock providers.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentEvent, AgentMiddleware, Approval, ChatRequest, ChatResponse,
    EventStream, Finish, Flow, Message, PendingToolCall, Provider, ProviderError, StopReason,
    StreamEvent, Tool, ToolCx, ToolError, ToolOutcome, Usage,
};
use stakit_model::{JsonSchema, Model};

// ── A hand-written tool (uses `ToolCx`, as tools always do) ─────────────────

#[derive(serde::Deserialize, Model, JsonSchema)]
struct EchoArgs {
    /// Text to echo back.
    text: String,
}

/// Echoes its `text` argument back.
struct EchoTool;

impl Tool<()> for EchoTool {
    type Args = EchoArgs;
    type Output = String;

    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echo the text back"
    }
    fn run<'a>(
        &'a self,
        _cx: &'a ToolCx<()>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        Box::pin(async move { Ok(args.text) })
    }
}

// ── A scripted provider: step 1 → tool call, step 2 → text "done" ───────────

#[derive(Clone)]
struct ScriptedProvider {
    calls: Arc<AtomicU32>,
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
}

impl ScriptedProvider {
    fn two_step() -> Self {
        Self {
            calls: Arc::new(AtomicU32::new(0)),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn first_seen(&self) -> usize {
        self.seen.lock().unwrap()[0]
    }
}

impl Provider for ScriptedProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "scripted"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move { Err(ProviderError::InvalidArgument("unused".into())) })
    }

    fn stream(&self, r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(r.messages.len());
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent, ProviderError>> = if n == 0 {
                vec![
                    Ok(StreamEvent::Start {
                        usage: Usage {
                            input_tokens: 10,
                            ..Usage::default()
                        },
                    }),
                    Ok(StreamEvent::ToolCall {
                        id: "t1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({ "text": "hi" }),
                    }),
                    Ok(StreamEvent::End {
                        stop: StopReason::ToolUse,
                        usage: Usage {
                            input_tokens: 10,
                            output_tokens: 5,
                            ..Usage::default()
                        },
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("done".into())),
                    Ok(StreamEvent::End {
                        stop: StopReason::EndTurn,
                        usage: Usage {
                            input_tokens: 20,
                            output_tokens: 3,
                            ..Usage::default()
                        },
                    }),
                ]
            };
            Ok(futures::stream::iter(events).boxed())
        })
    }
}

#[tokio::test]
async fn run_executes_tool_then_ends_and_streams() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);
    let mut run = agent.run();
    let mut saw_tool = false;
    let mut outcome = None;
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::ToolResult { .. } => saw_tool = true,
            AgentEvent::Done(o) => outcome = Some(o),
            _ => {}
        }
    }
    let out = outcome.unwrap();
    assert!(saw_tool);
    assert_eq!(out.text, "done");
    assert!(matches!(out.finish, Finish::EndTurn));
}

#[tokio::test]
async fn run_accumulates_usage_across_steps() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().await.expect("outcome");
    // input 10 + 20, output 5 + 3.
    assert_eq!(out.usage.input_tokens, 30);
    assert_eq!(out.usage.output_tokens, 8);
    assert_eq!(out.steps, 2);
    // The tool round-trip produced a result echoing "hi".
    let tool_result = agent.messages().iter().any(|m| {
        matches!(m, Message::User(parts) if parts.iter().any(|p| matches!(
            p,
            stakit_ai_sdk::UserContent::ToolResult { is_error: false, .. }
        )))
    });
    assert!(
        tool_result,
        "expected a tool-result message in the conversation"
    );
}

#[tokio::test]
async fn into_future_yields_outcome() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);
    // `IntoFuture`: await the run directly.
    let out = agent.run().await.expect("outcome");
    assert_eq!(out.text, "done");
}

// ── Middleware: deny a tool call → error result, loop continues ─────────────

struct DenyTools;

#[async_trait::async_trait]
impl AgentMiddleware<()> for DenyTools {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, ()>,
        call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        Ok(Approval::Deny {
            message: format!("{} not allowed", call.name),
        })
    }
}

#[tokio::test]
async fn denied_tool_yields_error_result_and_continues() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(DenyTools)
        .with_context(vec![Message::user("hi")]);
    let mut run = agent.run();
    let mut denied = false;
    let mut outcome = None;
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::ToolResult {
                result: ToolOutcome::Denied { message },
                ..
            } => {
                assert_eq!(message, "echo not allowed");
                denied = true;
            }
            AgentEvent::Done(o) => outcome = Some(o),
            _ => {}
        }
    }
    assert!(denied, "expected a denied tool result");
    // The loop still proceeds to the model's final turn.
    assert_eq!(outcome.unwrap().text, "done");
}

// ── Middleware: stop the run from on_step ───────────────────────────────────

struct StopAtStart;

#[async_trait::async_trait]
impl AgentMiddleware<()> for StopAtStart {
    async fn on_start(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        Ok(Flow::stop("halted before any model call"))
    }
}

#[tokio::test]
async fn middleware_stop_in_on_start_halts_with_message() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_middleware(StopAtStart)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().await.expect("outcome");
    assert!(matches!(out.finish, Finish::Stopped { .. }));
    assert_eq!(out.text, "halted before any model call");
    assert_eq!(out.steps, 0);
}

// ── Middleware on_start can load/prepend conversation (replaces ContextLoader)

struct SeedConversation;

#[async_trait::async_trait]
impl AgentMiddleware<()> for SeedConversation {
    async fn on_start(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        cx.messages_mut().splice(0..0, [Message::user("seeded")]);
        Ok(Flow::Continue)
    }
}

#[tokio::test]
async fn middleware_on_start_prepends_conversation() {
    let provider = ScriptedProvider::two_step();
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(SeedConversation)
        .with_context(vec![Message::user("hi")]);
    let _ = agent.run().await.expect("outcome");
    // seeded + hi were both present at the first provider call.
    assert_eq!(provider.first_seen(), 2);
}

// ── on_finish runs for every middleware whose on_start ran ───────────────────

#[derive(Clone)]
struct Marker {
    started: Arc<AtomicU32>,
    finished: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for Marker {
    async fn on_start(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(Flow::Continue)
    }
    async fn on_finish(&self, _cx: &AgentCx<'_, ()>) -> Result<(), AgentError> {
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn on_finish_runs_for_started_middleware() {
    let m = Marker {
        started: Arc::new(AtomicU32::new(0)),
        finished: Arc::new(AtomicU32::new(0)),
    };
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(m.clone())
        .with_context(vec![Message::user("hi")]);
    let _ = agent.run().await.expect("outcome");
    assert_eq!(m.started.load(Ordering::SeqCst), 1);
    assert_eq!(m.finished.load(Ordering::SeqCst), 1);
}

// ── Concurrent tool calls in one turn ───────────────────────────────────────

#[derive(Clone)]
struct ParallelProvider {
    calls: Arc<AtomicU32>,
}

impl Provider for ParallelProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "parallel"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async move { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<Result<StreamEvent, ProviderError>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolCall {
                        id: "a".into(),
                        name: "barrier".into(),
                        input: serde_json::json!({}),
                    }),
                    Ok(StreamEvent::ToolCall {
                        id: "b".into(),
                        name: "barrier".into(),
                        input: serde_json::json!({}),
                    }),
                    Ok(StreamEvent::End {
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("done".into())),
                    Ok(StreamEvent::End {
                        stop: StopReason::EndTurn,
                        usage: Usage::default(),
                    }),
                ]
            };
            Ok(futures::stream::iter(events).boxed())
        })
    }
}

#[derive(serde::Deserialize, Model, JsonSchema)]
struct NoArgs {}

#[derive(Clone)]
struct BarrierCtx {
    barrier: Arc<tokio::sync::Barrier>,
}

/// Blocks on a shared 2-party barrier — only completes if two instances run
/// concurrently.
struct BarrierTool;

impl Tool<BarrierCtx> for BarrierTool {
    type Args = NoArgs;
    type Output = String;

    fn name(&self) -> &'static str {
        "barrier"
    }
    fn description(&self) -> &'static str {
        "Wait on a shared barrier"
    }
    fn run<'a>(
        &'a self,
        cx: &'a ToolCx<BarrierCtx>,
        _args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        Box::pin(async move {
            cx.ctx().barrier.wait().await;
            Ok("released".into())
        })
    }
}

#[tokio::test]
async fn tool_calls_in_a_turn_run_concurrently() {
    let ctx = BarrierCtx {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
    };
    let mut agent = Agent::new(ctx)
        .provider(ParallelProvider {
            calls: Arc::new(AtomicU32::new(0)),
        })
        .model("parallel")
        .register_tool(BarrierTool)
        .with_context(vec![Message::user("go")]);

    // If the two tool calls ran sequentially, the first `barrier.wait()` would
    // block forever (count 1/2) and this would time out.
    let drive = async {
        let mut run = agent.run();
        let mut count = 0u32;
        while let Some(ev) = run.next().await {
            if let AgentEvent::ToolResult {
                result: ToolOutcome::Ok(_),
                ..
            } = ev
            {
                count += 1;
            }
        }
        count
    };
    let count = tokio::time::timeout(std::time::Duration::from_secs(5), drive)
        .await
        .expect("tools must run concurrently (otherwise deadlock)");
    assert_eq!(count, 2, "both tool calls should produce results");
}

// ── Cancellation (cooperative, via the run's cancel token) ──────────────────

/// Cancels the run from `on_start` (the loop observes it at the first step).
struct CancelImmediately;

#[async_trait::async_trait]
impl AgentMiddleware<()> for CancelImmediately {
    async fn on_start(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        cx.cancel_token().cancel();
        Ok(Flow::Continue)
    }
}

#[tokio::test]
async fn cancelled_run_finishes_cancelled() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(CancelImmediately)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().await.expect("outcome");
    assert!(
        matches!(out.finish, Finish::Cancelled),
        "expected Cancelled, got {:?}",
        out.finish
    );
    assert_eq!(out.steps, 0);
}

#[tokio::test]
async fn no_provider_for_model_stops_with_error() {
    let mut agent = Agent::new(())
        .model("ghost")
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().await.expect("outcome");
    assert!(matches!(out.finish, Finish::Stopped { .. }));
    let Finish::Stopped { message } = out.finish else {
        unreachable!()
    };
    assert!(
        message.unwrap_or_default().contains("ghost"),
        "expected the missing-model error"
    );
}

#[tokio::test]
async fn provider_sets_default_model() {
    let agent = Agent::new(()).provider(ScriptedProvider::two_step());
    assert_eq!(agent.current_model(), "scripted");
    // A second provider does not steal the default model.
    let agent = agent.register_provider(ParallelProvider {
        calls: Arc::new(AtomicU32::new(0)),
    });
    assert_eq!(agent.current_model(), "scripted");
}
