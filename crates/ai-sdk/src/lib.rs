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
mod cache;
mod cancel;
mod context;
mod cx;
mod error;
mod loop_event;
mod mcp;
mod message;
mod provider;
mod skill;
mod tool;
mod usage;

pub use agent::{Agent, AgentBuilder};
pub use cache::{CacheStrategy, CacheTarget, CacheTtl};
pub use cancel::CancelToken;
pub use context::{ContextLoader, FsContextLoader, LoadedContext};
pub use cx::{Permission, ToolCx};
pub use error::{AiError, ProviderError, ToolError};
pub use loop_event::{FinishReason, LoopEvent, StopCond};
pub use mcp::{McpConfig, McpServer, McpTool, McpToolSet, McpTransport};
pub use message::{
    AssistantContent, ImageSource, Message, SystemPrompt, Thinking, ToolResultPart, UserContent,
};
pub use provider::{
    ChatRequest, ChatResponse, EventStream, Provider, StopReason, StreamEvent, ThinkingConfig,
    ToolChoice, ToolDef, event_stream,
};
pub use skill::{FsSkillLoader, SkillContent, SkillLoader, SkillManifest};
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
