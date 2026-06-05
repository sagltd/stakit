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
                        arguments: r#"{ "text": "hi" }"#.into(),
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
                        arguments: "{}".into(),
                    }),
                    Ok(StreamEvent::ToolCall {
                        id: "b".into(),
                        name: "barrier".into(),
                        arguments: "{}".into(),
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

// ── Step cap: a tool-looping model stops at exactly DEFAULT_MAX_STEPS ────────

/// The library's default step cap. Kept in sync with `agent::DEFAULT_MAX_STEPS`;
/// asserted here so the loop runs exactly this many model calls (no off-by-one).
const DEFAULT_MAX_STEPS: u32 = 16;

/// Always emits a tool call (never ends its turn), forcing the step cap to fire.
#[derive(Clone)]
struct AlwaysToolProvider {
    calls: Arc<AtomicU32>,
}

impl Provider for AlwaysToolProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "always"
    }
    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(futures::stream::iter(vec![
                Ok(StreamEvent::ToolCall {
                    id: "x".into(),
                    name: "echo".into(),
                    arguments: r#"{"text":"x"}"#.into(),
                }),
                Ok(StreamEvent::End {
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                }),
            ])
            .boxed())
        })
    }
}

#[tokio::test]
async fn step_cap_runs_exactly_default_max_steps() {
    let calls = Arc::new(AtomicU32::new(0));
    let mut agent = Agent::new(())
        .provider(AlwaysToolProvider {
            calls: Arc::clone(&calls),
        })
        .model("always")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().await.expect("outcome");

    // Exactly DEFAULT_MAX_STEPS model calls and steps — no off-by-one.
    assert_eq!(
        calls.load(Ordering::SeqCst),
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

/// Emits a tool call whose argument text is invalid JSON on the first step.
#[derive(Clone)]
struct MalformedArgsProvider {
    calls: Arc<AtomicU32>,
}

impl Provider for MalformedArgsProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "malformed"
    }
    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let events = if n == 0 {
                vec![
                    // Truncated / invalid JSON — must NOT become `{}` silently.
                    Ok(StreamEvent::ToolCall {
                        id: "bad".into(),
                        name: "spy".into(),
                        arguments: r#"{"text": "oo"#.into(),
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

#[tokio::test]
async fn malformed_tool_args_yield_error_not_empty_call() {
    let spy = ArgSpy::default();
    let mut agent = Agent::new(())
        .provider(MalformedArgsProvider {
            calls: Arc::new(AtomicU32::new(0)),
        })
        .model("malformed")
        .register_tool(SpyTool { spy: spy.clone() })
        .with_context(vec![Message::user("hi")]);

    let mut run = agent.run();
    let mut tool_result = None;
    while let Some(ev) = run.next().await {
        if let AgentEvent::ToolResult { result, .. } = ev {
            tool_result = Some(result);
        }
    }

    match tool_result.expect("a tool result was produced") {
        ToolOutcome::Error(msg) => assert!(
            msg.contains("malformed tool arguments"),
            "expected a malformed-args error, got: {msg}"
        ),
        other => panic!("expected ToolOutcome::Error, got {other:?}"),
    }
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
#[derive(Clone)]
struct DrippleProvider {
    produced: Arc<AtomicU32>,
}

impl Provider for DrippleProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "ripple"
    }
    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let produced = Arc::clone(&self.produced);
        Box::pin(async move {
            let s = async_stream::stream! {
                for part in ["a", "b", "c"] {
                    // Yield so the consumer can run between each emission; a
                    // buffered loop would still drain all three before the host
                    // sees the first one.
                    tokio::task::yield_now().await;
                    produced.fetch_add(1, Ordering::SeqCst);
                    yield Ok(StreamEvent::TextDelta(part.into()));
                }
                produced.fetch_add(1, Ordering::SeqCst);
                yield Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                });
            };
            Ok(s.boxed())
        })
    }
}

#[tokio::test]
async fn streaming_deltas_are_yielded_live_and_in_order() {
    let produced = Arc::new(AtomicU32::new(0));
    let mut agent = Agent::new(())
        .provider(DrippleProvider {
            produced: Arc::clone(&produced),
        })
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

/// Calls `load_skill` with no `id` argument on the first step.
#[derive(Clone)]
struct LoadSkillNoIdProvider {
    calls: Arc<AtomicU32>,
}

impl Provider for LoadSkillNoIdProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "loadskill"
    }
    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let events = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolCall {
                        id: "ls".into(),
                        name: "load_skill".into(),
                        arguments: "{}".into(),
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

#[tokio::test]
async fn load_skill_without_id_errors() {
    let mut agent = Agent::new(())
        .provider(LoadSkillNoIdProvider {
            calls: Arc::new(AtomicU32::new(0)),
        })
        .model("loadskill")
        .skills(EmptyLoader)
        .with_context(vec![Message::user("hi")]);

    let mut run = agent.run();
    let mut tool_result = None;
    while let Some(ev) = run.next().await {
        if let AgentEvent::ToolResult { result, .. } = ev {
            tool_result = Some(result);
        }
    }
    match tool_result.expect("a tool result was produced") {
        ToolOutcome::Error(msg) => assert!(
            msg.contains("missing required argument: id"),
            "expected a missing-id error, got: {msg}"
        ),
        other => panic!("expected ToolOutcome::Error, got {other:?}"),
    }
}
