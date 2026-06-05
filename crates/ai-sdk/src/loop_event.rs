//! Events emitted by the agent loop, and the loop's stop conditions.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::control::Approval;
use crate::usage::Usage;

/// A condition that ends the loop after a step.
///
/// Budget and tool-call stops are a middleware concern; see [`AgentMiddleware`].
#[derive(Debug, Clone)]
pub enum StopCond {
    /// Stop once this many steps have run.
    StepCountIs(u32),
}

/// A streamed event from a running agent.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A new step started.
    StepStart {
        /// Zero-based step index.
        index: u32,
    },
    /// Reasoning (thinking) token delta.
    ReasoningDelta(String),
    /// Answer token delta.
    MessageDelta(String),
    /// The model requested a tool call.
    ToolCall {
        /// Provider-assigned call id.
        id: String,
        /// Tool name.
        name: String,
        /// Raw JSON arguments.
        args: Value,
    },
    /// A tool call resolved.
    ToolResult {
        /// Provider-assigned call id.
        id: String,
        /// Tool name.
        name: String,
        /// The result of the tool call.
        result: ToolOutcome,
    },
    /// A step finished.
    StepEnd {
        /// Zero-based step index.
        index: u32,
        /// Assistant answer text for this step.
        text: Arc<str>,
        /// Reasoning text, if any.
        reasoning: Option<Arc<str>>,
        /// Token usage for this step.
        usage: Usage,
        /// Estimated USD cost for this step, if pricing is known.
        cost: Option<f64>,
    },
    /// Terminal event.
    Done(Outcome),
}

/// What happened in one step (given to `on_step_done`).
#[derive(Debug, Clone)]
pub struct Step {
    /// Zero-based step index.
    pub index: u32,
    /// Reasoning text, if the model produced any.
    pub reasoning: Option<Arc<str>>,
    /// Assistant answer text for this step.
    pub text: Arc<str>,
    /// Tool calls resolved in this step.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Why the model ended this step.
    pub stop: crate::provider::StopReason,
}

/// A resolved tool call.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    /// Provider-assigned call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Raw JSON arguments.
    pub args: Value,
    /// What the middleware decided.
    pub approval: Approval,
    /// The result.
    pub result: ToolOutcome,
    /// Wall-clock duration.
    pub elapsed: Duration,
}

/// The outcome of a tool call.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    /// Success with a JSON result.
    Ok(Value),
    /// Denied by a middleware; message fed to the model.
    Denied {
        /// Reason returned to the model.
        message: String,
    },
    /// The tool errored.
    Error(String),
}

/// A pending tool call passed to `on_tool_approve`.
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    /// Provider-assigned call id.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Raw JSON arguments.
    pub args: Value,
}

/// The final result of a run.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Final assistant text (or a middleware stop message).
    pub text: String,
    /// Accumulated token usage.
    pub usage: Usage,
    /// Accumulated USD cost, if pricing is known.
    pub cost: Option<f64>,
    /// Number of steps taken.
    pub steps: u32,
    /// Why the run ended.
    pub finish: Finish,
}

/// Why a run ended.
#[derive(Debug, Clone)]
pub enum Finish {
    /// The model ended its turn.
    EndTurn,
    /// A stop condition fired (currently: step cap).
    Limit(StopCond),
    /// A middleware stopped the run.
    Stopped {
        /// Optional final message.
        message: Option<String>,
    },
    /// The run was cancelled.
    Cancelled,
}

#[cfg(test)]
mod new_type_tests {
    use super::*;

    #[test]
    fn outcome_and_event_shapes() {
        let out = Outcome {
            text: "hi".into(),
            usage: Usage::default(),
            cost: None,
            steps: 1,
            finish: Finish::EndTurn,
        };
        assert_eq!(out.steps, 1);
        assert!(matches!(
            AgentEvent::MessageDelta("a".into()),
            AgentEvent::MessageDelta(_)
        ));
        assert!(matches!(AgentEvent::Done(out), AgentEvent::Done(_)));
    }
}
