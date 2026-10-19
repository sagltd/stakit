//! Scripted providers, an echo tool and run helpers shared by the agent-loop
//! integration tests. Include with `#[path = "support/mock.rs"] mod mock;`.
#![allow(dead_code, reason = "each test crate uses a different subset")]

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use futures::future::BoxFuture;
use stakit_ai_sdk::{
    AgentEvent, AgentRun, ChatRequest, ChatResponse, EventStream, Finish, Outcome, Provider,
    ProviderError, StopReason, StreamEvent, Tool, ToolCx, ToolError, ToolOutcome, Usage,
    event_stream,
};
use stakit_model::{JsonSchema, Model};

type Script = dyn Fn(u32, &ChatRequest) -> Result<EventStream, ProviderError> + Send + Sync;

/// A provider that answers its n-th (0-based) streaming call with
/// `script(n, &request)`
/// and records every request; `complete` is unsupported. Clones share the call
/// counter and the request log.
#[derive(Clone)]
pub(crate) struct Scripted {
    model: &'static str,
    calls: Arc<AtomicU32>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    script: Arc<Script>,
}

impl Scripted {
    /// Serves `model` with full control over each call's stream (or error).
    pub(crate) fn new(
        model: &'static str,
        script: impl Fn(u32, &ChatRequest) -> Result<EventStream, ProviderError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            model,
            calls: Arc::new(AtomicU32::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(script),
        }
    }

    /// Serves `model`, streaming the events `turn(n)` for the n-th call.
    pub(crate) fn turns(
        model: &'static str,
        turn: impl Fn(u32) -> Vec<StreamEvent> + Send + Sync + 'static,
    ) -> Self {
        Self::new(model, move |call, _| Ok(stream_of(turn(call))))
    }

    /// Calls `tool` with `arguments` on the first step, then answers `text`.
    pub(crate) fn tool_then_text(
        model: &'static str,
        tool: &'static str,
        arguments: &'static str,
        text: &'static str,
    ) -> Self {
        Self::turns(model, move |call| {
            if call == 0 {
                tool_turn("t1", tool, arguments)
            } else {
                text_turn(text)
            }
        })
    }

    /// How many streaming calls the provider has served.
    pub(crate) fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every request received so far, in order.
    pub(crate) fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("request log").clone()
    }
}

impl Provider for Scripted {
    fn model_id(&self) -> &str {
        self.model
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let turn = (self.script)(self.calls.fetch_add(1, Ordering::SeqCst), &request);
        self.requests.lock().expect("request log").push(request);
        Box::pin(async move { turn })
    }
}

/// A stream yielding `events` in order.
pub(crate) fn stream_of(events: Vec<StreamEvent>) -> EventStream {
    event_stream(events.into_iter().map(Ok).collect())
}

/// A whole streamed tool call.
pub(crate) fn tool_call(id: &str, name: &str, arguments: &str) -> StreamEvent {
    StreamEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

/// A terminal event with zero usage.
pub(crate) fn end(stop: StopReason) -> StreamEvent {
    StreamEvent::End {
        stop,
        usage: Usage::default(),
    }
}

/// One step that calls a single tool.
pub(crate) fn tool_turn(id: &str, name: &str, arguments: &str) -> Vec<StreamEvent> {
    vec![tool_call(id, name, arguments), end(StopReason::ToolUse)]
}

/// One step that answers `text` and ends the turn.
pub(crate) fn text_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        end(StopReason::EndTurn),
    ]
}

#[derive(serde::Deserialize, Model, JsonSchema)]
pub(crate) struct EchoArgs {
    /// Text to echo back.
    pub(crate) text: String,
}

/// Echoes its `text` argument back, in any agent context.
pub(crate) struct EchoTool;

impl<C: Send + Sync + 'static> Tool<C> for EchoTool {
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
        _cx: &'a ToolCx<C>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        Box::pin(async move { Ok(args.text) })
    }
}

/// An agent context that counts how often the `effect` tool ran.
pub(crate) trait CountsEffects: Send + Sync + 'static {
    /// The effect counter.
    fn effects(&self) -> &AtomicUsize;
}

impl CountsEffects for AtomicUsize {
    fn effects(&self) -> &AtomicUsize {
        self
    }
}

#[derive(serde::Deserialize, Model, JsonSchema)]
pub(crate) struct NoArgs {}

/// The `effect` tool: records one side effect in the context's counter.
pub(crate) struct EffectTool;

impl<C: CountsEffects> Tool<Arc<C>> for EffectTool {
    type Args = NoArgs;
    type Output = &'static str;

    fn name(&self) -> &'static str {
        "effect"
    }
    fn description(&self) -> &'static str {
        "Record an effect."
    }
    fn run<'a>(
        &'a self,
        cx: &'a ToolCx<Arc<C>>,
        _args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        cx.ctx().effects().fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("done") })
    }
}

/// Drives a run to completion: every tool result in order, and the outcome.
pub(crate) async fn drain(mut run: AgentRun<'_>) -> (Vec<ToolOutcome>, Outcome) {
    let mut results = Vec::new();
    let mut outcome = None;
    while let Some(event) = run.next().await {
        match event {
            AgentEvent::ToolResult { result, .. } => results.push(result),
            AgentEvent::Done(done) => outcome = Some(done),
            _ => {}
        }
    }
    (results, outcome.expect("a run always ends with Done"))
}

/// The result of the run's last tool call.
pub(crate) async fn last_tool_result(run: AgentRun<'_>) -> ToolOutcome {
    let (mut results, _) = drain(run).await;
    results.pop().expect("a tool result was produced")
}

/// Asserts the run's last tool call failed with a message containing `needle`.
pub(crate) async fn assert_tool_error(run: AgentRun<'_>, needle: &str) {
    match last_tool_result(run).await {
        ToolOutcome::Error(message) => assert!(
            message.contains(needle),
            "expected an error containing {needle:?}, got: {message}"
        ),
        other => panic!("expected ToolOutcome::Error, got {other:?}"),
    }
}

/// Asserts the run was stopped (by middleware or an error), not finished.
pub(crate) fn assert_stopped(out: &Outcome) {
    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Finish::Stopped, got {:?}",
        out.finish
    );
}

/// Asserts the run stopped with a message containing `needle`.
pub(crate) fn assert_stopped_with(out: &Outcome, needle: &str) {
    assert_stopped(out);
    let Finish::Stopped { message } = &out.finish else {
        unreachable!("checked by assert_stopped")
    };
    let message = message.as_deref().unwrap_or_default();
    assert!(
        message.contains(needle),
        "expected {needle:?} in {message:?}"
    );
}
