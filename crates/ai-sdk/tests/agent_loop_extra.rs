//! Additional agent-loop tests covering:
//!   5. `Flow::Stop` from `on_step_done` halts after results are recorded.
//!   6. `Approval::Stop` from `on_tool_approve` halts the whole run (distinct from Deny).
//!   7. `set_system` mid-run: next provider request uses the new system prompt.
//!   8. An empty step never persists an empty assistant turn (resume-safety).
//!   9. A mid-batch `Approval::Stop` leaves no dangling `tool_use` (resume-safety).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentEvent, AgentMiddleware, Approval, AssistantContent,
    ChatRequest, ChatResponse, EventStream, Finish, Flow, Message, PendingToolCall, Provider,
    ProviderError, StopReason, StreamEvent, Tool, ToolCx, ToolError, ToolOutcome, Usage,
    UserContent,
};
use stakit_model::{JsonSchema, Model};

// ── Shared scripted/recording infrastructure ─────────────────────────────────

#[derive(Clone)]
struct TwoStepProvider {
    calls: Arc<AtomicU32>,
}

impl TwoStepProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl Provider for TwoStepProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "two-step"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let events: Vec<Result<StreamEvent, ProviderError>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolCall {
                        id: "tc1".into(),
                        name: "echo".into(),
                        arguments: r#"{"text":"hello"}"#.into(),
                    }),
                    Ok(StreamEvent::End {
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("final-text".into())),
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

#[derive(Clone)]
struct RecordingProvider {
    calls: Arc<AtomicU32>,
    system_log: Arc<Mutex<Vec<Option<String>>>>,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicU32::new(0)),
            system_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn systems(&self) -> Vec<Option<String>> {
        self.system_log.lock().unwrap().clone()
    }
}

impl Provider for RecordingProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "recorder"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.system_log
            .lock()
            .unwrap()
            .push(r.system.map(|s| s.text.to_string()));
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let events: Vec<Result<StreamEvent, ProviderError>> = if n == 0 {
                // First step: request a tool call so there's a second step.
                vec![
                    Ok(StreamEvent::ToolCall {
                        id: "tc1".into(),
                        name: "echo".into(),
                        arguments: r#"{"text":"hi"}"#.into(),
                    }),
                    Ok(StreamEvent::End {
                        stop: StopReason::ToolUse,
                        usage: Usage::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("ok".into())),
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
struct EchoArgs {
    /// Text to echo.
    text: String,
}

struct EchoTool;

impl Tool<()> for EchoTool {
    type Args = EchoArgs;
    type Output = String;

    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echo text"
    }
    fn run<'a>(
        &'a self,
        _cx: &'a ToolCx<()>,
        args: Self::Args,
    ) -> BoxFuture<'a, Result<Self::Output, ToolError>> {
        Box::pin(async move { Ok(args.text) })
    }
}

// ── Test 5: Flow::Stop from on_step_done halts after results recorded ─────────

struct StopInDone;

#[async_trait::async_trait]
impl AgentMiddleware<()> for StopInDone {
    async fn on_step_done(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        Ok(Flow::Stop("step-done-stop".into()))
    }
}

#[tokio::test]
async fn flow_stop_from_on_step_done_halts_after_results() {
    let mut agent = Agent::new(())
        .provider(TwoStepProvider::new())
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(StopInDone)
        .with_context(vec![Message::user("go")]);

    let mut run = agent.run();
    let mut saw_tool_result = false;
    let mut outcome = None;
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::ToolResult { .. } => saw_tool_result = true,
            AgentEvent::Done(o) => outcome = Some(o),
            _ => {}
        }
    }

    let out = outcome.unwrap();

    // The run must have recorded the tool result before stopping.
    assert!(
        saw_tool_result,
        "tool result must be emitted before on_step_done stop"
    );

    // Finish must be Stopped with the message from on_step_done.
    assert!(
        matches!(&out.finish, Finish::Stopped { message: Some(m) } if m == "step-done-stop"),
        "expected Finish::Stopped{{\"step-done-stop\"}}, got {:?}",
        out.finish
    );
    assert_eq!(out.text, "step-done-stop");
    // Exactly 1 step ran.
    assert_eq!(out.steps, 1);
}

// ── Test 6: Approval::Stop from on_tool_approve halts the whole run ──────────
//
// Approval::Stop is distinct from Approval::Deny: Deny produces a tool-result
// and the loop continues; Stop halts the entire run.

struct ApproveStop;

#[async_trait::async_trait]
impl AgentMiddleware<()> for ApproveStop {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, ()>,
        _call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        Ok(Approval::Stop {
            message: Some("approval-halted".to_string()),
        })
    }
}

#[tokio::test]
async fn approval_stop_halts_run_not_just_tool() {
    let calls = Arc::new(AtomicU32::new(0));
    let two_step = TwoStepProvider {
        calls: Arc::clone(&calls),
    };
    let mut agent = Agent::new(())
        .provider(two_step)
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(ApproveStop)
        .with_context(vec![Message::user("go")]);

    let mut run = agent.run();
    let mut saw_deny = false;
    let mut outcome = None;
    while let Some(ev) = run.next().await {
        match ev {
            // Deny would emit a ToolResult; Stop should not.
            AgentEvent::ToolResult {
                result: ToolOutcome::Denied { .. },
                ..
            } => saw_deny = true,
            AgentEvent::Done(o) => outcome = Some(o),
            _ => {}
        }
    }

    let out = outcome.unwrap();

    // Stop must have halted the run.
    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Finish::Stopped, got {:?}",
        out.finish
    );

    // Approval::Stop must NOT produce a Denied tool-result (only Deny does).
    assert!(
        !saw_deny,
        "Approval::Stop must not produce a Denied tool result"
    );

    // Provider must only have been called once (the second turn never happened).
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "provider must be called only once on Stop"
    );
}

// ── Test 7: set_system mid-run — next request uses the new system prompt ─────

struct SetSystemInStep {
    new_system: &'static str,
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for SetSystemInStep {
    async fn on_step(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        // Set the new system only on the second step (index 1) so we can
        // compare step 0 (original) vs step 1 (new).
        if cx.index() == 1 {
            cx.set_system(self.new_system);
        }
        Ok(Flow::Continue)
    }
}

#[tokio::test]
async fn set_system_mid_run_affects_next_provider_request() {
    let provider = RecordingProvider::new();
    let provider_clone = provider.clone();
    let mut agent = Agent::new(())
        .provider(provider)
        .model("recorder")
        .system("ORIGINAL")
        .register_tool(EchoTool)
        .register_middleware(SetSystemInStep { new_system: "NEW" })
        .with_context(vec![Message::user("go")]);

    let _ = agent.run().await.expect("outcome");

    let systems = provider_clone.systems();
    // There should be exactly 2 provider calls (tool step + final step).
    assert_eq!(systems.len(), 2, "expected exactly 2 provider calls");

    // Step 0: system is "ORIGINAL".
    let step0 = systems[0].as_deref().unwrap_or("");
    assert!(
        step0.contains("ORIGINAL"),
        "step 0 must use the original system prompt; got: {step0:?}"
    );

    // Step 1: set_system("NEW") was called in on_step, so the request must use "NEW".
    let step1 = systems[1].as_deref().unwrap_or("");
    assert!(
        step1.contains("NEW"),
        "step 1 must use the new system prompt after set_system; got: {step1:?}"
    );
    assert!(
        !step1.contains("ORIGINAL"),
        "step 1 must NOT contain the old system; got: {step1:?}"
    );
}

// ── Test 8 (C-1): an empty step never persists an empty assistant turn ────────
//
// A step that yields no text, reasoning, or tool calls must NOT push
// `Assistant(vec![])` — an `on_step_done` checkpoint would persist it and some
// providers 400 on resending an empty assistant turn.

/// Yields a single empty step (just `End`, no content).
#[derive(Clone)]
struct EmptyStepProvider;

impl Provider for EmptyStepProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "empty"
    }
    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async move {
            Ok(futures::stream::iter(vec![Ok(StreamEvent::End {
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            })])
            .boxed())
        })
    }
}

#[tokio::test]
async fn empty_step_does_not_persist_empty_assistant_turn() {
    let mut agent = Agent::new(())
        .provider(EmptyStepProvider)
        .model("empty")
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");
    assert!(matches!(out.finish, Finish::EndTurn));

    // No assistant message may be empty (resume-safety).
    let empty_assistant = agent
        .messages()
        .iter()
        .any(|m| matches!(m, Message::Assistant(blocks) if blocks.is_empty()));
    assert!(
        !empty_assistant,
        "an empty step must not persist an empty assistant turn: {:?}",
        agent.messages()
    );
}

// ── Test 9 (C-2): mid-batch Approval::Stop leaves no dangling tool_use ────────
//
// The assistant turn carries N `tool_use` blocks; a `Stop` on call k records
// `tool_result`s only for the calls that ran. The history must still have a
// matching `tool_result` for EVERY `tool_use` id (Claude rejects a resumed turn
// with dangling tool_use blocks).

/// Emits two tool calls in one turn on the first step.
#[derive(Clone)]
struct TwoToolProvider {
    calls: Arc<AtomicU32>,
}

impl Provider for TwoToolProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "two-tool"
    }
    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(futures::stream::iter(vec![
                Ok(StreamEvent::ToolCall {
                    id: "call_a".into(),
                    name: "echo".into(),
                    arguments: r#"{"text":"a"}"#.into(),
                }),
                Ok(StreamEvent::ToolCall {
                    id: "call_b".into(),
                    name: "echo".into(),
                    arguments: r#"{"text":"b"}"#.into(),
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

/// Stops on the FIRST tool approval (mid-batch — the second call never runs).
struct StopOnFirstApproval;

#[async_trait::async_trait]
impl AgentMiddleware<()> for StopOnFirstApproval {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, ()>,
        _call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        Ok(Approval::Stop {
            message: Some("stopped mid-batch".to_string()),
        })
    }
}

#[tokio::test]
async fn mid_batch_stop_records_tool_result_for_every_tool_use() {
    let mut agent = Agent::new(())
        .provider(TwoToolProvider {
            calls: Arc::new(AtomicU32::new(0)),
        })
        .model("two-tool")
        .register_tool(EchoTool)
        .register_middleware(StopOnFirstApproval)
        .with_context(vec![Message::user("go")]);

    let out = agent.run().await.expect("outcome");
    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Finish::Stopped, got {:?}",
        out.finish
    );

    // Collect every tool_use id from assistant turns and every tool_result id
    // from user turns.
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut tool_result_ids: Vec<String> = Vec::new();
    for m in agent.messages() {
        match m {
            Message::Assistant(blocks) => {
                for b in blocks {
                    if let AssistantContent::ToolUse { id, .. } = b {
                        tool_use_ids.push(id.to_string());
                    }
                }
            }
            Message::User(parts) => {
                for p in parts {
                    if let UserContent::ToolResult { id, .. } = p {
                        tool_result_ids.push(id.to_string());
                    }
                }
            }
        }
    }

    assert_eq!(
        tool_use_ids,
        vec!["call_a".to_string(), "call_b".to_string()],
        "both tool_use blocks must be persisted"
    );
    // Every tool_use id must have a matching tool_result id (no dangling calls).
    for id in &tool_use_ids {
        assert!(
            tool_result_ids.contains(id),
            "tool_use id {id} has no matching tool_result; results={tool_result_ids:?}"
        );
    }
}
