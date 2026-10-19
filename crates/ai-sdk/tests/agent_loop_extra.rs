//! Additional agent-loop tests covering:
//!   5. `Flow::Stop` from `on_step_done` halts after results are recorded.
//!   6. `Approval::Stop` from `on_tool_approve` halts the whole run (distinct from Deny).
//!   7. `set_system` mid-run: next provider request uses the new system prompt.
//!   8. An empty step never persists an empty assistant turn (resume-safety).
//!   9. A mid-batch `Approval::Stop` leaves no dangling `tool_use` (resume-safety).

use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, Approval, AssistantContent, Finish, Flow, Message,
    PendingToolCall, StopReason, ToolOutcome, UserContent,
};

#[path = "support/mock.rs"]
mod mock;
use mock::{EchoTool, Scripted, drain, end, tool_call};

// ── Shared scripted/recording infrastructure ─────────────────────────────────

fn two_step() -> Scripted {
    Scripted::tool_then_text("two-step", "echo", r#"{"text":"hello"}"#, "final-text")
}

/// The system prompt text of every request `provider` received.
fn systems(provider: &Scripted) -> Vec<Option<String>> {
    provider
        .requests()
        .into_iter()
        .map(|request| request.system.map(|system| system.text.to_string()))
        .collect()
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
        .provider(two_step())
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(StopInDone)
        .with_context(vec![Message::user("go")]);

    let (results, out) = drain(agent.run()).await;

    // The run must have recorded the tool result before stopping.
    assert!(
        !results.is_empty(),
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
    let two_step = two_step();
    let mut agent = Agent::new(())
        .provider(two_step.clone())
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(ApproveStop)
        .with_context(vec![Message::user("go")]);

    let (results, out) = drain(agent.run()).await;

    // Stop must have halted the run.
    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Finish::Stopped, got {:?}",
        out.finish
    );

    // Approval::Stop must NOT produce a Denied tool-result (only Deny does).
    assert!(
        !results
            .iter()
            .any(|result| matches!(result, ToolOutcome::Denied { .. })),
        "Approval::Stop must not produce a Denied tool result"
    );

    // Provider must only have been called once (the second turn never happened).
    assert_eq!(
        two_step.calls(),
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
    // First step: request a tool call so there's a second step.
    let provider = Scripted::tool_then_text("recorder", "echo", r#"{"text":"hi"}"#, "ok");
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .system("ORIGINAL")
        .register_tool(EchoTool)
        .register_middleware(SetSystemInStep { new_system: "NEW" })
        .with_context(vec![Message::user("go")]);

    let _ = agent.run().await.expect("outcome");

    let systems = systems(&provider);
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

#[tokio::test]
async fn empty_step_does_not_persist_empty_assistant_turn() {
    let mut agent = Agent::new(())
        // Yields a single empty step (just `End`, no content).
        .provider(Scripted::turns("empty", |_| vec![end(StopReason::EndTurn)]))
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
        // Emits two tool calls in one turn on every step.
        .provider(Scripted::turns("two-tool", |_| {
            vec![
                tool_call("call_a", "echo", r#"{"text":"a"}"#),
                tool_call("call_b", "echo", r#"{"text":"b"}"#),
                end(StopReason::ToolUse),
            ]
        }))
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
