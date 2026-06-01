//! Events emitted by the agent loop, and the loop's stop conditions.

use serde_json::Value;

use crate::provider::StopReason;
use crate::usage::Usage;

/// An event yielded by [`Agent::run`](crate::Agent::run).
#[derive(Debug, Clone, PartialEq)]
pub enum LoopEvent {
    /// A new step (provider round-trip) began.
    StepStart {
        /// 1-based step number.
        step: u32,
    },
    /// A chunk of assistant text.
    TextDelta(String),
    /// A chunk of reasoning text.
    ReasoningDelta(String),
    /// The model requested a tool call.
    ToolCall {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed arguments.
        input: Value,
    },
    /// A tool finished (or was denied); the result was appended to history.
    ToolResult {
        /// Tool-call id this result answers.
        id: String,
        /// Tool output (or the error/denial message when `is_error`).
        output: Value,
        /// Whether this is an error result.
        is_error: bool,
    },
    /// Per-step token usage and estimated cost.
    Usage {
        /// Step number.
        step: u32,
        /// Usage for this step.
        usage: Usage,
        /// Estimated USD cost for this step, if pricing is known.
        cost: Option<f64>,
    },
    /// A step finished.
    StepEnd {
        /// Step number.
        step: u32,
        /// Why the model stopped this step.
        stop: StopReason,
    },
    /// The loop finished.
    Done {
        /// The final assistant text.
        text: String,
        /// Cumulative usage across the run.
        usage: Usage,
        /// Cumulative estimated USD cost, if pricing is known.
        cost: Option<f64>,
        /// Why the loop ended.
        reason: FinishReason,
    },
}

/// Why the agent loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// The model ended its turn with no tool calls.
    EndTurn,
    /// A `stop_when` condition matched.
    StopCondition,
    /// The step-count limit was reached.
    MaxSteps,
    /// The budget limit was reached.
    MaxBudget,
    /// The run was cancelled.
    Cancelled,
    /// A provider or fatal error ended the run.
    Error,
}

/// A condition that ends the loop after a step (OR-ed together).
#[derive(Debug, Clone)]
pub enum StopCond {
    /// Stop once this many steps have run.
    StepCountIs(u32),
    /// Stop if a tool with this name was called.
    HasToolCall(String),
    /// Stop once cumulative estimated cost reaches this many USD.
    BudgetUsd(f64),
}
