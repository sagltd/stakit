//! `stakit-ai-sdk` — provider-agnostic primitives for building LLM agents.
//!
//! This crate is a toolbox, not a fixed agent: a `Provider` abstraction over
//! LLM backends (Claude, `OpenAI`, or your own), a typed tool system, an
//! injectable agent loop, MCP and skill loading, and token-usage + cost
//! telemetry. It depends on nothing from `stakit-router`; integration with a
//! router (or any host) happens through the agent's generic context type.
//!
//! Build order: this crate currently provides the core data model (messages,
//! usage/cost, cache strategy, errors); providers, tools and the agent loop
//! layer on top.

mod agent;
mod agent_cx;
mod cache;
mod cancel;
mod control;
mod cx;
mod error;
mod llm;
mod loop_event;
mod mcp;
mod message;
mod middleware;
mod provider;
mod retry;
mod skill;
mod tool;
mod usage;

pub use agent::{Agent, AgentRun};
pub use agent_cx::AgentCx;
pub use cache::{CacheStrategy, CacheTarget, CacheTtl};
pub use cancel::CancelToken;
pub use control::{Approval, Flow};
pub use cx::ToolCx;
pub use error::{AgentError, ProviderError, ToolError};
pub use llm::LLM;
pub use loop_event::{
    AgentEvent, Finish, Outcome, PendingToolCall, Step, StopCond, ToolCallRecord, ToolOutcome,
};
pub use mcp::{McpConfig, McpServer, McpTool, McpToolSet, McpTransport};
pub use message::{
    AssistantContent, Image, Message, SystemPrompt, Thinking, ToolResultPart, UserContent,
};
pub use middleware::AgentMiddleware;
pub use provider::{
    ChatRequest, ChatResponse, EventStream, Provider, StopReason, StreamEvent, ThinkingConfig,
    ToolChoice, ToolDef, event_stream,
};
pub use retry::{RetryPolicy, Retryable, classify};
pub use skill::{Skill, SkillContent, SkillLoader};
pub use tool::{Tool, ToolDyn, ToolRegistry, ToolSet, TypedTool};
pub use usage::{ModelPrice, Pricing, Usage};

/// A boxed, `Send` future — the return type of [`Tool::run`].
pub use futures::future::BoxFuture;

/// The `#[tool]` attribute macro.
pub use stakit_ai_sdk_derive::tool;

/// The built-in Anthropic Claude provider.
#[cfg(feature = "claude")]
pub use provider::claude::{ClaudeClient, ClaudeModel};

/// The built-in `OpenAI` provider (Chat Completions).
#[cfg(feature = "openai")]
pub use provider::openai::{OpenAiClient, OpenAiModel};

/// Internal test hooks for offline provider-body/cache assertions. Not part of
/// the stable public API; shapes may change without notice.
#[doc(hidden)]
pub mod test_support {
    /// Builds the Anthropic request body for a [`crate::ChatRequest`].
    #[cfg(feature = "claude")]
    pub use crate::provider::claude::build_request_body as claude_body;
    /// Builds the `OpenAI` request body for a [`crate::ChatRequest`].
    #[cfg(feature = "openai")]
    pub use crate::provider::openai::build_request_body as openai_body;
}
