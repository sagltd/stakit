//! Composition-rule tests for multiple middleware stacks.
//!
//! Each test uses ≥2 middlewares to verify the locked composition guarantees from
//! the design spec §7:
//! - Forward (registration) order in `on_start` / `on_step` / `on_step_done`
//! - First `Flow::Stop` wins and skips later middlewares in the same hook
//! - `on_finish` runs for every middleware whose `on_start` already ran (even on stop/error)
//! - `on_tool_approve` most-restrictive precedence: Stop > Deny > Allow

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, Approval, Finish, Flow, Message, PendingToolCall,
    ToolOutcome,
};

#[path = "support/mock.rs"]
mod mock;
use mock::{EchoTool, Scripted, assert_stopped, last_tool_result};

// ── Shared app context that carries the event log ───────────────────────────

#[derive(Clone, Default)]
struct LogCtx {
    log: Arc<Mutex<Vec<String>>>,
}

impl LogCtx {
    fn push(&self, entry: impl Into<String>) {
        self.log.lock().unwrap().push(entry.into());
    }

    fn entries(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

// ── A simple 2-step scripted provider ───────────────────────────────────────

fn two_step() -> Scripted {
    Scripted::tool_then_text("two-step", "echo", r#"{"text":"hello"}"#, "finished")
}

// ── Test 1: Forward order in on_start / on_step / on_step_done ──────────────

struct LogHook {
    name: &'static str,
}

#[async_trait::async_trait]
impl AgentMiddleware<LogCtx> for LogHook {
    async fn on_start(&self, cx: &mut AgentCx<'_, LogCtx>) -> Result<Flow, AgentError> {
        cx.ctx().push(format!("on_start:{}", self.name));
        Ok(Flow::Continue)
    }
    async fn on_step(&self, cx: &mut AgentCx<'_, LogCtx>) -> Result<Flow, AgentError> {
        cx.ctx().push(format!("on_step:{}", self.name));
        Ok(Flow::Continue)
    }
    async fn on_step_done(&self, cx: &mut AgentCx<'_, LogCtx>) -> Result<Flow, AgentError> {
        cx.ctx().push(format!("on_step_done:{}", self.name));
        Ok(Flow::Continue)
    }
    async fn on_finish(&self, cx: &AgentCx<'_, LogCtx>) -> Result<(), AgentError> {
        cx.ctx().push(format!("on_finish:{}", self.name));
        Ok(())
    }
}

#[tokio::test]
async fn hooks_run_in_registration_order() {
    let ctx = LogCtx::default();
    let mut agent = Agent::new(ctx.clone())
        .provider(two_step())
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(LogHook { name: "A" })
        .register_middleware(LogHook { name: "B" })
        .with_context(vec![Message::user("go")]);

    let _ = agent.run().await.expect("outcome");

    let entries = ctx.entries();

    // on_start: A before B.
    let sa = entries
        .iter()
        .position(|e| e == "on_start:A")
        .expect("on_start:A");
    let sb = entries
        .iter()
        .position(|e| e == "on_start:B")
        .expect("on_start:B");
    assert!(sa < sb, "on_start must run A before B");

    // on_step: A before B (may appear multiple times for multi-step runs).
    let step_a = entries
        .iter()
        .position(|e| e == "on_step:A")
        .expect("on_step:A");
    let step_b = entries
        .iter()
        .position(|e| e == "on_step:B")
        .expect("on_step:B");
    assert!(step_a < step_b, "on_step must run A before B");

    // on_step_done: A before B.
    let sda = entries
        .iter()
        .position(|e| e == "on_step_done:A")
        .expect("on_step_done:A");
    let sdb = entries
        .iter()
        .position(|e| e == "on_step_done:B")
        .expect("on_step_done:B");
    assert!(sda < sdb, "on_step_done must run A before B");

    // on_finish: A before B.
    let fa = entries
        .iter()
        .position(|e| e == "on_finish:A")
        .expect("on_finish:A");
    let fb = entries
        .iter()
        .position(|e| e == "on_finish:B")
        .expect("on_finish:B");
    assert!(fa < fb, "on_finish must run A before B");
}

// ── Test 2: First Flow::Stop in on_step wins, B's on_step does NOT run ───────

struct StopInStep {
    name: &'static str,
    b_ran: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for StopInStep {
    async fn on_step(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        if self.name == "A" {
            return Ok(Flow::Stop("a".into()));
        }
        // If B ever runs on_step, record it.
        self.b_ran.store(true, Ordering::SeqCst);
        Ok(Flow::Continue)
    }
    async fn on_finish(&self, _cx: &AgentCx<'_, ()>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[tokio::test]
async fn first_stop_in_on_step_skips_later_middlewares() {
    let b_ran = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::new(())
        .provider(two_step())
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(StopInStep {
            name: "A",
            b_ran: Arc::clone(&b_ran),
        })
        .register_middleware(StopInStep {
            name: "B",
            b_ran: Arc::clone(&b_ran),
        })
        .with_context(vec![Message::user("go")]);

    let out = agent.run().await.expect("outcome");

    // The run must end Stopped with text "a".
    assert!(
        matches!(&out.finish, Finish::Stopped { message: Some(m) } if m == "a"),
        "expected Finish::Stopped{{message:\"a\"}}, got {:?}",
        out.finish
    );
    assert_eq!(out.text, "a");

    // B's on_step must never have run.
    assert!(
        !b_ran.load(Ordering::SeqCst),
        "B's on_step must not run after A returns Stop"
    );
}

// ── Test 3a: on_finish runs for all started middlewares even after a stop ────
//
// Stop fires in on_step (both on_start already ran) → both A and B get on_finish.

/// Logs `start:` / `finish:` with its name; optionally stops in `on_start`
/// ("halted") or `on_step` ("stopped").
struct Tracker {
    name: &'static str,
    /// Shared log for sequencing assertions.
    log: Arc<Mutex<Vec<String>>>,
    stop_in_start: bool,
    stop_in_step: bool,
}

impl Tracker {
    fn log(&self, hook: &str) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{hook}:{}", self.name));
    }
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for Tracker {
    async fn on_start(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        self.log("start");
        if self.stop_in_start {
            return Ok(Flow::Stop("halted".into()));
        }
        Ok(Flow::Continue)
    }
    async fn on_step(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        if self.stop_in_step {
            return Ok(Flow::Stop("stopped".into()));
        }
        Ok(Flow::Continue)
    }
    async fn on_finish(&self, _cx: &AgentCx<'_, ()>) -> Result<(), AgentError> {
        self.log("finish");
        Ok(())
    }
}

/// Runs trackers A (stopping as configured) then B, which never stops; asserts
/// the run stopped and returns the shared log.
async fn tracked_run(a_stops_in_start: bool, a_stops_in_step: bool) -> Vec<String> {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let tracker = |name, stop_in_start, stop_in_step| Tracker {
        name,
        log: Arc::clone(&log),
        stop_in_start,
        stop_in_step,
    };
    let mut agent = Agent::new(())
        .provider(two_step())
        .model("two-step")
        .register_tool(EchoTool)
        .register_middleware(tracker("A", a_stops_in_start, a_stops_in_step))
        .register_middleware(tracker("B", false, false))
        .with_context(vec![Message::user("go")]);
    assert_stopped(&agent.run().await.expect("outcome"));
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn on_finish_runs_for_all_started_middlewares_after_step_stop() {
    let entries = tracked_run(false, true).await;
    // Both on_start ran → both on_finish must appear.
    assert!(entries.contains(&"start:A".to_string()), "start:A missing");
    assert!(entries.contains(&"start:B".to_string()), "start:B missing");
    assert!(
        entries.contains(&"finish:A".to_string()),
        "finish:A missing after stop"
    );
    assert!(
        entries.contains(&"finish:B".to_string()),
        "finish:B missing after stop"
    );
}

// ── Test 3b: stop in on_start — later ones do NOT get on_finish ──────────────

#[tokio::test]
async fn on_finish_only_for_started_middlewares_when_on_start_stops() {
    // Middleware A stops in on_start → B's on_start never runs.
    // Expected: finish:A runs, finish:B does NOT.
    let entries = tracked_run(true, false).await;
    assert!(
        entries.contains(&"start:A".to_string()),
        "start:A must be present"
    );
    assert!(
        !entries.contains(&"start:B".to_string()),
        "start:B must NOT have run"
    );
    assert!(
        entries.contains(&"finish:A".to_string()),
        "finish:A must run after A's on_start"
    );
    assert!(
        !entries.contains(&"finish:B".to_string()),
        "finish:B must NOT run (B never started)"
    );
}

// ── Test 4: on_tool_approve — Deny beats Allow, Stop beats Deny ─────────────

// A provider that produces a single tool call then ends.
fn single_tool() -> Scripted {
    Scripted::tool_then_text("single-tool", "echo", r#"{"text":"hi"}"#, "done")
}

/// Answers every tool approval with a fixed decision.
struct Approver(Approval);

#[async_trait::async_trait]
impl AgentMiddleware<()> for Approver {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, ()>,
        _call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        Ok(self.0.clone())
    }
}

/// A single-tool agent whose approvals are `first` then `second`.
fn approving(first: Approval, second: Approval) -> Agent<()> {
    Agent::new(())
        .provider(single_tool())
        .model("single-tool")
        .register_tool(EchoTool)
        .register_middleware(Approver(first))
        .register_middleware(Approver(second))
        .with_context(vec![Message::user("go")])
}

fn deny(message: &str) -> Approval {
    Approval::Deny {
        message: message.into(),
    }
}

fn stop() -> Approval {
    Approval::Stop {
        message: Some("stop-from-approve".to_string()),
    }
}

#[tokio::test]
async fn deny_beats_allow_in_tool_approve() {
    // A=Allow, B=Deny → effective approval is Deny.
    let mut agent = approving(Approval::Allow, deny("not-allowed"));
    match last_tool_result(agent.run()).await {
        ToolOutcome::Denied { message } => {
            assert_eq!(
                message, "not-allowed",
                "Deny from B must win over Allow from A"
            );
        }
        other => panic!("expected Denied, got {other:?}"),
    }
}

#[tokio::test]
async fn stop_beats_deny_and_allow_in_tool_approve() {
    // A=Deny or Allow, B=Stop → effective approval is Stop (run halts).
    for first in [deny("denied"), Approval::Allow] {
        let mut agent = approving(first, stop());
        assert_stopped(&agent.run().await.expect("outcome"));
    }
}
