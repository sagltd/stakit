//! End-to-end tests for the stateful agent loop, driven by mock providers.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentEvent, AgentMiddleware, Approval, Finish, Flow, Message,
    PendingToolCall, StopReason, StreamEvent, Tool, ToolCx, ToolError, ToolOutcome, Usage,
};
use stakit_model::{JsonSchema, Model};

#[path = "support/mock.rs"]
mod mock;
use mock::{EchoTool, Scripted, assert_tool_error, drain, end, text_turn, tool_turn};

// ── A scripted provider: step 1 → tool call, step 2 → text "done" ───────────

fn two_step() -> Scripted {
    Scripted::turns("scripted", |call| {
        let usage = |input_tokens, output_tokens| Usage {
            input_tokens,
            output_tokens,
            ..Usage::default()
        };
        if call == 0 {
            vec![
                StreamEvent::Start {
                    usage: usage(10, 0),
                },
                mock::tool_call("t1", "echo", r#"{ "text": "hi" }"#),
                StreamEvent::End {
                    stop: StopReason::ToolUse,
                    usage: usage(10, 5),
                },
            ]
        } else {
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: usage(20, 3),
                },
            ]
        }
    })
}

#[tokio::test]
async fn run_executes_tool_then_ends_and_streams() {
    let mut agent = Agent::new(())
        .provider(two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);
    let (results, out) = drain(agent.run()).await;
    assert!(!results.is_empty());
    assert_eq!(out.text, "done");
    assert!(matches!(out.finish, Finish::EndTurn));
}

#[tokio::test]
async fn run_accumulates_usage_across_steps() {
    let mut agent = Agent::new(())
        .provider(two_step())
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
        .provider(two_step())
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
        .provider(two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(DenyTools)
        .with_context(vec![Message::user("hi")]);
    let (results, out) = drain(agent.run()).await;
    assert!(
        matches!(results.as_slice(), [ToolOutcome::Denied { message }] if message == "echo not allowed"),
        "expected a denied tool result, got {results:?}"
    );
    // The loop still proceeds to the model's final turn.
    assert_eq!(out.text, "done");
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
        .provider(two_step())
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
    let provider = two_step();
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(SeedConversation)
        .with_context(vec![Message::user("hi")]);
    let _ = agent.run().await.expect("outcome");
    // seeded + hi were both present at the first provider call.
    assert_eq!(provider.requests()[0].messages.len(), 2);
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
        .provider(two_step())
        .model("scripted")
        .register_tool(EchoTool)
        .register_middleware(m.clone())
        .with_context(vec![Message::user("hi")]);
    let _ = agent.run().await.expect("outcome");
    assert_eq!(m.started.load(Ordering::SeqCst), 1);
    assert_eq!(m.finished.load(Ordering::SeqCst), 1);
}

// ── Concurrent tool calls in one turn ───────────────────────────────────────

fn parallel() -> Scripted {
    Scripted::turns("parallel", |call| {
        if call == 0 {
            vec![
                mock::tool_call("a", "barrier", "{}"),
                mock::tool_call("b", "barrier", "{}"),
                end(StopReason::ToolUse),
            ]
        } else {
            text_turn("done")
        }
    })
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
        .provider(parallel())
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
        .provider(two_step())
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
    let agent = Agent::new(()).provider(two_step());
    assert_eq!(agent.current_model(), "scripted");
    // A second provider does not steal the default model.
    let agent = agent.register_provider(parallel());
    assert_eq!(agent.current_model(), "scripted");
}

// ── Step cap: a tool-looping model stops at exactly DEFAULT_MAX_STEPS ────────

/// The library's default step cap. Kept in sync with `agent::DEFAULT_MAX_STEPS`;
/// asserted here so the loop runs exactly this many model calls (no off-by-one).
const DEFAULT_MAX_STEPS: u32 = 16;

/// Always emits a tool call (never ends its turn), forcing the step cap to fire.
fn always_tool() -> Scripted {
    Scripted::turns("always", |_| tool_turn("x", "echo", r#"{"text":"x"}"#))
}

#[tokio::test]
async fn step_cap_runs_exactly_default_max_steps() {
    let provider = always_tool();
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("always")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().await.expect("outcome");

    // Exactly DEFAULT_MAX_STEPS model calls and steps — no off-by-one.
    assert_eq!(
        provider.calls(),
        DEFAULT_MAX_STEPS,
        "provider must be called exactly DEFAULT_MAX_STEPS times"
    );
    assert_eq!(out.steps, DEFAULT_MAX_STEPS);
    assert!(
        matches!(
            out.finish,
            Finish::Limit(stakit_ai_sdk::StopCond::StepCountIs(n)) if n == DEFAULT_MAX_STEPS
        ),
        "expected Finish::Limit(StepCountIs({DEFAULT_MAX_STEPS})), got {:?}",
        out.finish
    );
}

// ── Malformed tool arguments → ToolOutcome::Error, never a silent {} call ────

/// Records every argument set its tool body actually received (to prove a
/// malformed-args call never reaches the body).
#[derive(Clone, Default)]
struct ArgSpy {
    seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

#[derive(serde::Deserialize, Model, JsonSchema)]
struct AnyArgs {
    /// A required string field.
    text: String,
}

struct SpyTool {
    spy: ArgSpy,
}

impl Tool<()> for SpyTool {
    type Args = AnyArgs;
    type Output = String;

    fn name(&self) -> &'static str {
        "spy"
    }
    fn description(&self) -> &'static str {
        "Records the args it was called with"
    }
    fn run<'a>(
        &'a self,
        _cx: &'a ToolCx<()>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        let spy = self.spy.clone();
        Box::pin(async move {
            spy.seen
                .lock()
                .unwrap()
                .push(serde_json::json!({ "text": args.text }));
            Ok("ok".into())
        })
    }
}

#[tokio::test]
async fn malformed_tool_args_yield_error_not_empty_call() {
    let spy = ArgSpy::default();
    let mut agent = Agent::new(())
        // Truncated / invalid JSON — must NOT become `{}` silently.
        .provider(Scripted::tool_then_text(
            "malformed",
            "spy",
            r#"{"text": "oo"#,
            "done",
        ))
        .model("malformed")
        .register_tool(SpyTool { spy: spy.clone() })
        .with_context(vec![Message::user("hi")]);
    assert_tool_error(agent.run(), "malformed tool arguments").await;
    // The tool body must never have been invoked with coerced `{}` args.
    assert!(
        spy.seen.lock().unwrap().is_empty(),
        "tool body must not be called when arguments are malformed"
    );
}

// ── Live streaming: deltas reach the host as they are produced ───────────────

/// Emits text deltas one at a time, bumping a shared counter as each leaves the
/// provider. The consumer compares "how many the provider has produced" against
/// "how many it has received" at the moment of the first delta — proving the
/// loop forwards each event live, not buffered until the turn ends.
fn dripple(produced: &Arc<AtomicU32>) -> Scripted {
    let produced = Arc::clone(produced);
    Scripted::new("ripple", move |_| {
        let produced = Arc::clone(&produced);
        Ok(async_stream::stream! {
            for part in ["a", "b", "c"] {
                // Yield so the consumer can run between each emission; a
                // buffered loop would still drain all three before the host
                // sees the first one.
                tokio::task::yield_now().await;
                produced.fetch_add(1, Ordering::SeqCst);
                yield Ok(StreamEvent::TextDelta(part.into()));
            }
            produced.fetch_add(1, Ordering::SeqCst);
            yield Ok(end(StopReason::EndTurn));
        }
        .boxed())
    })
}

#[tokio::test]
async fn streaming_deltas_are_yielded_live_and_in_order() {
    let produced = Arc::new(AtomicU32::new(0));
    let mut agent = Agent::new(())
        .provider(dripple(&produced))
        .model("ripple")
        .with_context(vec![Message::user("hi")]);

    let mut run = agent.run();
    let mut deltas = Vec::new();
    let mut produced_at_first_delta = None;
    let mut delta_count_at_step_end = None;
    let mut assembled = String::new();
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::MessageDelta(d) => {
                if produced_at_first_delta.is_none() {
                    produced_at_first_delta = Some(produced.load(Ordering::SeqCst));
                }
                assembled.push_str(&d);
                deltas.push(d);
            }
            AgentEvent::StepEnd { text, .. } => {
                delta_count_at_step_end = Some(deltas.len());
                assert_eq!(&*text, "abc", "StepEnd text is the assembled deltas");
            }
            _ => {}
        }
    }

    // Liveness: when the host saw the first delta, the provider had produced
    // exactly one event. A buffered loop would have drained all four (a, b, c,
    // End) before yielding anything, so this would be 4.
    assert_eq!(
        produced_at_first_delta,
        Some(1),
        "first delta must reach the host before the provider produces the rest"
    );
    assert_eq!(deltas, vec!["a", "b", "c"], "deltas must arrive in order");
    assert_eq!(assembled, "abc");
    assert_eq!(delta_count_at_step_end, Some(3));
}

// ── load_skill with a missing id short-circuits to an error ──────────────────

struct EmptyLoader;

#[async_trait::async_trait]
impl stakit_ai_sdk::SkillLoader<()> for EmptyLoader {
    async fn list(&self, _ctx: &()) -> Result<Vec<stakit_ai_sdk::Skill>, AgentError> {
        Ok(vec![stakit_ai_sdk::Skill {
            id: "s1".into(),
            name: "Skill One".into(),
            description: "first".into(),
        }])
    }
    async fn load(&self, _ctx: &(), id: &str) -> Result<stakit_ai_sdk::SkillContent, AgentError> {
        // Should never be reached with an empty id.
        assert!(!id.is_empty(), "loader must not be called with an empty id");
        Ok(stakit_ai_sdk::SkillContent {
            body: format!("body for {id}"),
            references: Vec::new(),
        })
    }
}

#[tokio::test]
async fn load_skill_without_id_errors() {
    let mut agent = Agent::new(())
        // Calls `load_skill` with no `id` argument on the first step.
        .provider(Scripted::tool_then_text(
            "loadskill",
            "load_skill",
            "{}",
            "done",
        ))
        .model("loadskill")
        .skills(EmptyLoader)
        .with_context(vec![Message::user("hi")]);
    assert_tool_error(agent.run(), "missing required argument: id").await;
}
