# Agent Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `stakit-ai-sdk` into a stateful, cheap-to-build `Agent<Ctx>` driven by one `agent.run()` loop, one `AgentMiddleware` trait (conversation load/save + control), a dyn provider registry with runtime model switching, automatic prompt caching, and streaming `AgentEvent`s — with no runner/session machinery in the SDK.

**Architecture:** `Agent<Ctx>` is a stateful session object holding app `ctx`, a provider registry, current model, tools, skills (name+desc), middleware, the `messages` conversation, cache + retry config, and accumulated usage. `run(&mut self)` runs the agentic loop, mutating `messages`. The conversation is loaded/saved/compacted entirely by host middleware. Sessions are reconstructed (new agent + load conversation), never stored by the SDK.

**Tech Stack:** Rust (edition 2024, workspace), `tokio`, `futures`, `async-stream`, `async-trait` (new dep), `reqwest`, `serde`, `indexmap`, `tracing`. Tests via `cargo-nextest`. Spec: `docs/superpowers/specs/2026-06-04-agent-refactor-design.md`.

**Conventions for every task:** run tests with `cargo nextest run -p stakit-ai-sdk -E 'test(<name>)'`; doctests with `cargo test -p stakit-ai-sdk --doc`; full gate `./code-check.sh`. Clippy is `-D warnings` (pedantic+nursery), `unsafe` forbidden, public items need docs. Commit after each task.

> **Note on phasing:** The crate compiles as a whole, so some tasks intentionally leave `#[allow(dead_code)]` or temporary shims until a later phase wires them in; each task says when. Phases 1–9 build the new surface; Phase 10 deletes the old surface and flips `lib.rs`; Phase 11 is acceptance tests. Review between phases.

---

## Phase 0: Prep

### Task 0.1: Add dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root `[workspace.dependencies]`)
- Modify: `crates/ai-sdk/Cargo.toml`

- [ ] **Step 1: Check newest `async-trait` version**

Run: `cargo search async-trait` (note the latest `0.1.x`). Per project memory, pin the newest.

- [ ] **Step 2: Add to workspace deps**

In root `Cargo.toml` under `[workspace.dependencies]`, add (use the version from Step 1):

```toml
async-trait = "0.1.83"
```

- [ ] **Step 3: Pull into the crate**

In `crates/ai-sdk/Cargo.toml` `[dependencies]`, add:

```toml
async-trait.workspace = true
tracing.workspace = true
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p stakit-ai-sdk`
Expected: builds clean (no usage yet).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/ai-sdk/Cargo.toml Cargo.lock
git commit -m "chore(ai-sdk): add async-trait + tracing deps for agent refactor"
```

---

## Phase 1: Foundation types — `Message`, `Image`

`message.rs` currently has `Message`, `UserContent`, `AssistantContent`, `SystemPrompt`, `Thinking`, `ImageSource`. Make content cheap to clone (`Arc<str>`) and replace `ImageSource` with a provider-neutral `Image`.

### Task 1.1: Add the `Image` enum

**Files:**
- Modify: `crates/ai-sdk/src/message.rs`
- Test: `crates/ai-sdk/src/message.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the tests module in `message.rs`:

```rust
#[test]
fn image_url_is_cheap_clone_and_serializes_as_url() {
    let img = Image::Url("https://x/y.png".into());
    let clone = img.clone();
    assert!(matches!(clone, Image::Url(ref u) if &**u == "https://x/y.png"));
    let json = serde_json::to_value(&img).unwrap();
    assert_eq!(json, serde_json::json!({ "type": "url", "url": "https://x/y.png" }));
}

#[test]
fn image_base64_roundtrips() {
    let img = Image::Base64 { media_type: "image/png".into(), data: bytes::Bytes::from_static(b"\x89PNG") };
    let json = serde_json::to_value(&img).unwrap();
    let back: Image = serde_json::from_value(json).unwrap();
    assert!(matches!(back, Image::Base64 { .. }));
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(image_)'`
Expected: FAIL — `Image` not found.

- [ ] **Step 3: Implement `Image`**

In `message.rs`, add (and `use std::sync::Arc; use bytes::Bytes;`):

```rust
/// A provider-neutral image reference inside a message.
///
/// Prefer [`Image::Url`] / [`Image::FileId`]: base64 is re-sent every turn and
/// bloats both the request and the prompt cache. Each provider adapter maps this
/// to its own wire format at send time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Image {
    /// A public URL the provider fetches.
    Url(Arc<str>),
    /// An id from a provider Files API (uploaded once, referenced cheaply).
    FileId(Arc<str>),
    /// Inline base64 bytes; use only when no URL/file id exists.
    Base64 {
        /// MIME type, e.g. `image/png`.
        media_type: Arc<str>,
        /// Raw bytes (cheap to clone).
        data: Bytes,
    },
}
```

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(image_)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/message.rs
git commit -m "feat(ai-sdk): add provider-neutral Image enum (url/file_id/base64)"
```

### Task 1.2: Switch `Message` text content to `Arc<str>` and replace `ImageSource` with `Image`

**Files:**
- Modify: `crates/ai-sdk/src/message.rs`
- Modify: `crates/ai-sdk/src/provider/claude.rs`, `crates/ai-sdk/src/provider/openai.rs` (mapping sites)
- Test: `crates/ai-sdk/src/message.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn user_message_text_is_arc_backed_and_cheap_to_clone() {
    let m = Message::user("hello");
    let c = m.clone(); // must not deep-copy the string
    assert_eq!(format!("{:?}", c).contains("hello"), true);
}
```

- [ ] **Step 2: Run, verify it fails / compiles wrong**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(user_message_text_is_arc)'`
Expected: FAIL or compile error if `Message::user` returns owned `String` content.

- [ ] **Step 3: Implement**

In `message.rs`: change text-bearing variants of `UserContent` / `AssistantContent` to hold `Arc<str>` instead of `String`; update `Message::user`/`assistant` constructors to accept `impl Into<Arc<str>>`. Replace `ImageSource` usages in `UserContent` with `Image` (delete `ImageSource`). Keep `serde` representations stable where providers expect them.

- [ ] **Step 4: Fix provider mapping sites**

In `provider/claude.rs` and `provider/openai.rs`, update where messages/images are serialized to read the new `Arc<str>` text and `Image` variants (map `Image::Url`→provider url block, `Image::Base64`→provider base64 block, `Image::FileId`→provider file ref). Compile-guided.

- [ ] **Step 5: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(message)'` then `cargo build -p stakit-ai-sdk`
Expected: PASS + builds.

- [ ] **Step 6: Commit**

```bash
git add crates/ai-sdk/src/message.rs crates/ai-sdk/src/provider/
git commit -m "refactor(ai-sdk): Arc<str> message content + Image replaces ImageSource"
```

---

## Phase 2: Provider → object-safe + dyn registry

`provider.rs` has `trait Provider { type Raw; fn model_id; fn complete; fn stream; }`. The agent drops the `P` generic and stores `IndexMap<String, Box<dyn Provider>>`, so `Provider` must be object-safe (no associated `Raw`, methods return `BoxFuture`).

### Task 2.1: Make `Provider` object-safe

**Files:**
- Modify: `crates/ai-sdk/src/provider.rs`
- Test: `crates/ai-sdk/src/provider.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn provider_is_object_safe_and_dispatches_via_dyn() {
    let p: Box<dyn Provider> = Box::new(MockProvider::default()); // MockProvider already in tests
    assert_eq!(p.model_id(), "mock");
    let _stream = p.stream(ChatRequest::default()).await.unwrap();
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(provider_is_object_safe)'`
Expected: FAIL — `Provider` not object-safe (associated type / RPITIT).

- [ ] **Step 3: Implement**

In `provider.rs`, rewrite the trait to be dyn-safe; drop `type Raw` (return parsed `Usage`/content; keep raw JSON in an internal field only if needed):

```rust
/// A chat-completion backend. Object-safe so the agent can hold
/// `Box<dyn Provider>` and switch providers at runtime.
pub trait Provider: Send + Sync {
    /// The model id this provider serves (registry key).
    fn model_id(&self) -> &str;

    /// One non-streaming completion.
    fn complete<'a>(&'a self, req: ChatRequest)
        -> futures::future::BoxFuture<'a, Result<ChatResponse, ProviderError>>;

    /// A streaming completion.
    fn stream<'a>(&'a self, req: ChatRequest)
        -> futures::future::BoxFuture<'a, Result<EventStream, ProviderError>>;
}
```

Replace `ChatResponse<R>` with a non-generic `ChatResponse { content, stop, usage }` (drop `raw: R`; if a raw-JSON escape hatch is still wanted, add `raw: serde_json::Value`).

- [ ] **Step 4: Update Claude + OpenAI impls**

In `provider/claude.rs` and `provider/openai.rs`, change `impl Provider for ClaudeModel`/`OpenAiModel` to the new signatures (wrap bodies in `Box::pin(async move { ... })`). Remove `type Raw`.

- [ ] **Step 5: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(provider)'` then `cargo build -p stakit-ai-sdk --all-features`
Expected: PASS + builds.

- [ ] **Step 6: Commit**

```bash
git add crates/ai-sdk/src/provider.rs crates/ai-sdk/src/provider/
git commit -m "refactor(ai-sdk): make Provider object-safe (BoxFuture, no assoc Raw)"
```

---

## Phase 3: Control + event types

New/renamed types in their own modules. These compile standalone (mark `#[allow(dead_code)]` until wired).

### Task 3.1: `AgentError`, `Flow`, `Approval`

**Files:**
- Modify: `crates/ai-sdk/src/error.rs`
- Create: `crates/ai-sdk/src/control.rs`
- Modify: `crates/ai-sdk/src/lib.rs` (add `mod control;`)
- Test: `crates/ai-sdk/src/control.rs`

- [ ] **Step 1: Write the failing test**

In `control.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flow_stop_carries_message() {
        let f = Flow::stop("done");
        assert!(matches!(f, Flow::Stop(ref m) if m == "done"));
    }
    #[test]
    fn approval_variants_exist() {
        let _ = (Approval::Allow, Approval::Deny { message: "no".into() }, Approval::Stop { message: None });
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(flow_stop_carries)'`
Expected: FAIL — module/types missing.

- [ ] **Step 3: Implement**

Create `control.rs`:

```rust
//! Middleware control-flow types.

/// Returned by conversation-phase middleware hooks.
#[derive(Debug, Clone)]
pub enum Flow {
    /// Keep running.
    Continue,
    /// Halt the run; the string becomes the final response text.
    Stop(String),
}
impl Flow {
    /// Stop with a final message.
    pub fn stop(msg: impl Into<String>) -> Self { Self::Stop(msg.into()) }
}

/// Returned by `on_tool_approve`.
#[derive(Debug, Clone)]
pub enum Approval {
    /// Run the tool.
    Allow,
    /// Skip it; feed `message` to the model as the tool result; loop continues.
    Deny {
        /// Reason returned to the model.
        message: String,
    },
    /// Halt the whole agent; optional final text.
    Stop {
        /// Optional final response text.
        message: Option<String>,
    },
}
```

In `error.rs`, rename `AiError` → `AgentError` (keep `ProviderError`, `ToolError`); add a `context` constructor:

```rust
impl AgentError {
    /// Wrap a host context (db) failure.
    pub fn context(e: impl std::fmt::Display) -> Self { /* map to an AgentError::Context variant */ }
}
```

Add `mod control;` to `lib.rs`.

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(flow_stop_carries)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/control.rs crates/ai-sdk/src/error.rs crates/ai-sdk/src/lib.rs
git commit -m "feat(ai-sdk): Flow/Approval control types; rename AiError->AgentError"
```

### Task 3.2: `AgentEvent`, `Outcome`, `Finish`, `Step`, `ToolCallRecord`, `ToolOutcome`, `PendingToolCall`

**Files:**
- Modify: `crates/ai-sdk/src/loop_event.rs` (rename to event types; keep `StopCond`)
- Test: `crates/ai-sdk/src/loop_event.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn outcome_and_event_shapes() {
    let out = Outcome { text: "hi".into(), usage: Usage::default(), cost: None, steps: 1, finish: Finish::EndTurn };
    assert_eq!(out.steps, 1);
    let _ev = AgentEvent::MessageDelta("a".into());
    let _done = AgentEvent::Done(out);
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(outcome_and_event_shapes)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `loop_event.rs`, replace `LoopEvent`/`FinishReason` with:

```rust
use crate::{control::Approval, message::Message, usage::Usage};
use serde_json::Value;
use std::time::Duration;

/// A streamed event from a running agent.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A new step started.
    StepStart { index: u32 },
    /// Reasoning (thinking) token delta.
    ReasoningDelta(String),
    /// Answer token delta.
    MessageDelta(String),
    /// The model requested a tool call.
    ToolCall { id: String, name: String, args: Value },
    /// A tool call resolved.
    ToolResult { id: String, name: String, result: ToolOutcome },
    /// A step finished.
    StepEnd { index: u32, text: String, reasoning: Option<String>, usage: Usage, cost: Option<f64> },
    /// Terminal event.
    Done(Outcome),
}

/// What happened in one step (given to `on_step_done`).
#[derive(Debug, Clone)]
pub struct Step {
    pub index: u32,
    pub reasoning: Option<String>,
    pub text: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub stop: crate::provider::StopReason,
}

/// A resolved tool call.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub approval: Approval,
    pub result: ToolOutcome,
    pub elapsed: Duration,
}

/// The outcome of a tool call.
#[derive(Debug, Clone)]
pub enum ToolOutcome { Ok(Value), Denied { message: String }, Error(String) }

/// A pending tool call passed to `on_tool_approve`.
#[derive(Debug, Clone)]
pub struct PendingToolCall { pub id: String, pub name: String, pub args: Value }

/// The final result of a run.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub text: String,
    pub usage: Usage,
    pub cost: Option<f64>,
    pub steps: u32,
    pub finish: Finish,
}

/// Why a run ended.
#[derive(Debug, Clone)]
pub enum Finish {
    EndTurn,
    Limit(StopCond),
    Stopped { message: Option<String> },
    Cancelled,
}
```

Keep `StopCond` as-is. Add doc comments to satisfy `missing_docs`.

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(outcome_and_event_shapes)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/loop_event.rs
git commit -m "feat(ai-sdk): AgentEvent/Outcome/Finish/Step/ToolOutcome types"
```

---

## Phase 4: `AgentCx` + `AgentMiddleware`

`cx.rs` has `ToolCx<Ctx>` (ctx + cancel) and `Permission`. Rename to `AgentCx`, extend with conversation/model accessors; replace `Permission` (done by `Approval`).

### Task 4.1: `AgentCx<Ctx>`

**Files:**
- Modify: `crates/ai-sdk/src/cx.rs`
- Test: `crates/ai-sdk/src/cx.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn agentcx_exposes_ctx_messages_and_model() {
    let mut msgs = vec![Message::user("hi")];
    let mut model = String::from("gpt-5");
    let usage = Usage::default();
    let mut cx = AgentCx::for_test(&7u32, &mut msgs, &mut model, &usage);
    assert_eq!(*cx.ctx(), 7);
    cx.messages_mut().push(Message::user("again"));
    assert_eq!(cx.messages().len(), 2);
    cx.set_model("claude-opus");
    assert_eq!(cx.model(), "claude-opus");
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(agentcx_exposes)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

Rewrite `cx.rs`. `AgentCx<'a, Ctx>` borrows the agent's mutable run state (built by `run()` via borrow-splitting):

```rust
use crate::{cancel::CancelToken, loop_event::Step, message::Message, usage::Usage};

/// Context handed to tools (shared) and middleware (mutable) during a run.
pub struct AgentCx<'a, Ctx> {
    ctx: &'a Ctx,
    messages: &'a mut Vec<Message>,
    model: &'a mut String,
    system: &'a mut Option<String>,
    usage: &'a Usage,
    cost: Option<f64>,
    index: u32,
    last_step: Option<&'a Step>,
    cancel: &'a CancelToken,
}

impl<'a, Ctx> AgentCx<'a, Ctx> {
    /// The app context (db/user/session).
    pub fn ctx(&self) -> &Ctx { self.ctx }
    /// The conversation (borrowed).
    pub fn messages(&self) -> &[Message] { self.messages }
    /// The conversation (mutable) — load/inject/compact.
    pub fn messages_mut(&mut self) -> &mut Vec<Message> { self.messages }
    /// Accumulated usage.
    pub fn usage(&self) -> &Usage { self.usage }
    /// Accumulated USD cost (if pricing known).
    pub fn cost(&self) -> Option<f64> { self.cost }
    /// Current step index.
    pub fn index(&self) -> u32 { self.index }
    /// The last completed step (in `on_step_done`).
    pub fn step(&self) -> Option<&Step> { self.last_step }
    /// The active model id.
    pub fn model(&self) -> &str { self.model }
    /// Switch model+provider for the rest of the run.
    pub fn set_model(&mut self, id: impl Into<String>) { *self.model = id.into(); }
    /// Switch system prompt for the rest of the run.
    pub fn set_system(&mut self, text: impl Into<String>) { *self.system = Some(text.into()); }
    /// Cancellation token (for cooperative tool cancellation).
    pub fn cancel_token(&self) -> &CancelToken { self.cancel }
    /// True if the run was cancelled.
    pub fn is_cancelled(&self) -> bool { self.cancel.is_cancelled() }
}
```

Add a `#[cfg(test)] pub fn for_test(...)` constructor matching the test. Delete `Permission`.

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(agentcx_exposes)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/cx.rs
git commit -m "refactor(ai-sdk): ToolCx -> AgentCx with messages/model accessors; drop Permission"
```

### Task 4.2: `AgentMiddleware<Ctx>` trait

**Files:**
- Create: `crates/ai-sdk/src/middleware.rs`
- Modify: `crates/ai-sdk/src/lib.rs` (`mod middleware;`)
- Test: `crates/ai-sdk/src/middleware.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    struct Counting(std::sync::atomic::AtomicU32);
    #[async_trait::async_trait]
    impl AgentMiddleware<()> for Counting {
        async fn on_step(&self, _cx: &mut crate::cx::AgentCx<'_, ()>) -> Result<Flow, AgentError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Flow::Continue)
        }
    }
    #[tokio::test]
    async fn middleware_default_hooks_are_noops() {
        struct Empty;
        #[async_trait::async_trait] impl AgentMiddleware<()> for Empty {}
        let m = Empty;
        let mut msgs = vec![]; let mut model = "m".to_string(); let u = Usage::default();
        let mut cx = crate::cx::AgentCx::for_test(&(), &mut msgs, &mut model, &u);
        assert!(matches!(m.on_start(&mut cx).await.unwrap(), Flow::Continue));
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(middleware_default_hooks)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

Create `middleware.rs`:

```rust
//! The single agent extension trait.
use crate::{control::{Approval, Flow}, cx::AgentCx, error::AgentError, loop_event::PendingToolCall};

/// Conversation load/save, tool approval, stop, and model/system switching.
#[async_trait::async_trait]
pub trait AgentMiddleware<Ctx>: Send + Sync + 'static {
    /// Before the first model call. Load the conversation / inject guidance.
    async fn on_start(&self, _cx: &mut AgentCx<'_, Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// Before each model call. Switch model/system, check budget, compact, drain queued input.
    async fn on_step(&self, _cx: &mut AgentCx<'_, Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// After each step resolves. Persist / observe.
    async fn on_step_done(&self, _cx: &mut AgentCx<'_, Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// Gate every tool call.
    async fn on_tool_approve(&self, _cx: &AgentCx<'_, Ctx>, _call: &PendingToolCall) -> Result<Approval, AgentError> { Ok(Approval::Allow) }
    /// After the loop ends (any reason). Persist final / cleanup.
    async fn on_finish(&self, _cx: &AgentCx<'_, Ctx>) -> Result<(), AgentError> { Ok(()) }
}
```

Add `mod middleware;` to `lib.rs`.

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(middleware_)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/middleware.rs crates/ai-sdk/src/lib.rs
git commit -m "feat(ai-sdk): AgentMiddleware trait (on_start/step/step_done/tool_approve/finish)"
```

---

## Phase 5: Skills — `SkillLoader` (host impl)

`skill.rs` has `SkillLoader { list, load }`, `SkillManifest`, `SkillContent`, `FsSkillLoader`. Slim to `Skill { id, name, description }`, keep the trait keyed off the app `Ctx`, remove `FsSkillLoader`.

### Task 5.1: Reshape `Skill` + `SkillLoader`, delete `FsSkillLoader`

**Files:**
- Modify: `crates/ai-sdk/src/skill.rs`
- Test: `crates/ai-sdk/src/skill.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn skill_loader_lists_and_loads() {
    struct Mem;
    #[async_trait::async_trait]
    impl SkillLoader<()> for Mem {
        async fn list(&self, _ctx: &()) -> Result<Vec<Skill>, AgentError> {
            Ok(vec![Skill { id: "a".into(), name: "Alpha".into(), description: "does alpha".into() }])
        }
        async fn load(&self, _ctx: &(), id: &str) -> Result<SkillContent, AgentError> {
            assert_eq!(id, "a");
            Ok(SkillContent { body: "BODY".into(), references: vec![] })
        }
    }
    let m = Mem;
    assert_eq!(m.list(&()).await.unwrap()[0].name, "Alpha");
    assert_eq!(m.load(&(), "a").await.unwrap().body, "BODY");
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(skill_loader_lists)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

Rewrite `skill.rs`:

```rust
//! Host-supplied skills (name + description; body loaded on demand).
use crate::error::AgentError;

/// A skill manifest entry (no body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill { pub id: String, pub name: String, pub description: String }

/// A loaded skill body.
#[derive(Debug, Clone)]
pub struct SkillContent { pub body: String, pub references: Vec<String> }

/// Source of skills — db, fs, anywhere. The agent caches `list()` and loads
/// bodies on demand via the built-in `load_skill` tool.
#[async_trait::async_trait]
pub trait SkillLoader<Ctx>: Send + Sync + 'static {
    /// All available skills (name + description only).
    async fn list(&self, ctx: &Ctx) -> Result<Vec<Skill>, AgentError>;
    /// Fetch one skill's body by id.
    async fn load(&self, ctx: &Ctx, id: &str) -> Result<SkillContent, AgentError>;
}
```

Delete `FsSkillLoader`, `SkillManifest`, the YAML frontmatter parser, and their tests.

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(skill)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/skill.rs
git commit -m "refactor(ai-sdk): slim SkillLoader (Skill{id,name,description}); remove FsSkillLoader"
```

---

## Phase 6: Stateful `Agent<Ctx>` + builder

Rewrite `agent.rs`. Replace `Agent<P, Ctx>` + `AgentBuilder` + `Inner` with a single stateful `Agent<Ctx>`. This is the largest task; split into builder (6.1) and run loop (6.2).

### Task 6.1: `Agent<Ctx>` struct + builder methods (no run yet)

**Files:**
- Modify: `crates/ai-sdk/src/agent.rs`
- Test: `crates/ai-sdk/src/agent.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn builder_registers_providers_tools_middleware_and_defaults() {
    let agent = Agent::new(())                          // app ctx = ()
        .provider(MockProvider::with_id("gpt-5"))
        .register_provider(MockProvider::with_id("claude-opus"))
        .model("gpt-5")
        .system("sys")
        .with_context(vec![Message::user("hi")]);
    assert_eq!(agent.current_model(), "gpt-5");
    assert_eq!(agent.messages().len(), 1);
    assert!(agent.has_provider("claude-opus"));
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(builder_registers)'`
Expected: FAIL.

- [ ] **Step 3: Implement the struct + builder**

Replace the top of `agent.rs`:

```rust
use indexmap::IndexMap;
use std::{sync::Arc, time::Duration};
use crate::{
    cache::CacheStrategy, message::Message, middleware::AgentMiddleware,
    provider::Provider, skill::{Skill, SkillLoader}, tool::{Tool, ToolDyn, TypedTool},
    usage::{Pricing, Usage},
};

type CacheKeyFn<Ctx> = Arc<dyn Fn(&Ctx) -> Option<Arc<str>> + Send + Sync>;

/// A stateful agent session: providers, tools, skills, middleware, conversation.
pub struct Agent<Ctx> {
    ctx: Ctx,
    providers: IndexMap<String, Box<dyn Provider>>,
    current_model: String,
    tools: Vec<Box<dyn ToolDyn<Ctx>>>,
    skills: Vec<Skill>,
    skill_loader: Option<Box<dyn SkillLoader<Ctx>>>,
    middleware: Arc<[Box<dyn AgentMiddleware<Ctx>>]>,
    middleware_build: Vec<Box<dyn AgentMiddleware<Ctx>>>, // staging before build freeze
    system: Option<String>,
    messages: Vec<Message>,
    cache: CacheStrategy,
    cache_key: Option<CacheKeyFn<Ctx>>,
    retry: crate::retry::RetryPolicy,
    pricing: Pricing,
    usage: Usage,
    max_tokens: u32,
}

impl<Ctx: Send + Sync + 'static> Agent<Ctx> {
    /// New agent with the given app context.
    pub fn new(ctx: Ctx) -> Self {
        Self {
            ctx, providers: IndexMap::new(), current_model: String::new(),
            tools: Vec::new(), skills: Vec::new(), skill_loader: None,
            middleware: Arc::from(Vec::new()), middleware_build: Vec::new(),
            system: None, messages: Vec::new(), cache: CacheStrategy::Auto,
            cache_key: None, retry: crate::retry::RetryPolicy::default(),
            pricing: Pricing::new(), usage: Usage::default(), max_tokens: 4096,
        }
    }

    /// Register a provider and make it the default.
    pub fn provider(mut self, p: impl Provider + 'static) -> Self {
        let id = p.model_id().to_string();
        if self.current_model.is_empty() { self.current_model = id.clone(); }
        self.providers.insert(id, Box::new(p));
        self
    }
    /// Register an additional provider.
    pub fn register_provider(mut self, p: impl Provider + 'static) -> Self {
        self.providers.insert(p.model_id().to_string(), Box::new(p));
        self
    }
    /// Set the default model id (must be registered).
    pub fn model(mut self, id: impl Into<String>) -> Self { self.current_model = id.into(); self }
    /// Set the default system prompt.
    pub fn system(mut self, s: impl Into<String>) -> Self { self.system = Some(s.into()); self }
    /// Register one tool.
    pub fn register_tool<T: Tool<Ctx>>(mut self, t: T) -> Self { self.tools.push(Box::new(TypedTool(t))); self }
    /// Register many tools.
    pub fn register_tools<T: Tool<Ctx>, I: IntoIterator<Item = T>>(mut self, it: I) -> Self {
        for t in it { self.tools.push(Box::new(TypedTool(t))); } self
    }
    /// Register a middleware (runs in registration order).
    pub fn register_middleware<M: AgentMiddleware<Ctx>>(mut self, m: M) -> Self {
        self.middleware_build.push(Box::new(m)); self
    }
    /// Set the skill loader.
    pub fn skills<L: SkillLoader<Ctx>>(mut self, l: L) -> Self { self.skill_loader = Some(Box::new(l)); self }
    /// Seed the conversation.
    pub fn with_context(mut self, msgs: Vec<Message>) -> Self { self.messages = msgs; self }
    /// Prompt-cache configuration.
    pub fn cache(mut self, c: CacheStrategy) -> Self { self.cache = c; self }
    /// Prompt-cache key derived from the app context.
    pub fn cache_key(mut self, f: impl Fn(&Ctx) -> Option<Arc<str>> + Send + Sync + 'static) -> Self {
        self.cache_key = Some(Arc::new(f)); self
    }
    /// Max retry attempts.
    pub fn with_retries(mut self, n: u32) -> Self { self.retry.max_retries = n; self }
    /// Per-attempt timeout.
    pub fn with_timeout(mut self, d: Duration) -> Self { self.retry.timeout = d; self }
    /// Pricing table for cost.
    pub fn pricing(mut self, p: Pricing) -> Self { self.pricing = p; self }

    /// Add a message to the conversation before `run()`.
    pub fn push(&mut self, m: Message) { self.messages.push(m); }
    /// The active model id.
    pub fn current_model(&self) -> &str { &self.current_model }
    /// The conversation.
    pub fn messages(&self) -> &[Message] { &self.messages }
    /// Accumulated usage.
    pub fn usage(&self) -> &Usage { &self.usage }
    #[cfg(test)] pub fn has_provider(&self, id: &str) -> bool { self.providers.contains_key(id) }

    fn freeze_middleware(&mut self) {
        if !self.middleware_build.is_empty() {
            let mut v = std::mem::take(&mut self.middleware_build);
            let mut existing: Vec<_> = self.middleware.iter().map(|_| unreachable!()).collect(); // first build only
            let _ = &mut existing;
            self.middleware = Arc::from(std::mem::take(&mut v).into_boxed_slice());
        }
    }
}
```

> Note: the `freeze_middleware` shim converts the staging `Vec` to the shared `Arc<[_]>` on first `run()`; simplify to just `self.middleware = Arc::from(v.into_boxed_slice())` (drop `middleware` as a separate field if you keep only the staging Vec and clone an `Arc` at run start). Keep whichever compiles cleanly — the goal is: builder pushes to a Vec, `run()` borrows it as a cloned `Arc<[_]>` so the rest of `self` can be `&mut`.

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(builder_registers)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/agent.rs
git commit -m "feat(ai-sdk): stateful Agent<Ctx> struct + builder (no run yet)"
```

### Task 6.2: `RetryPolicy` (needed by builder)

**Files:**
- Create: `crates/ai-sdk/src/retry.rs`
- Modify: `crates/ai-sdk/src/lib.rs` (`mod retry;`)
- Test: `crates/ai-sdk/src/retry.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_classification() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 2);
        assert!(p.is_retryable(&Retryable::Transient));
        assert!(!p.is_retryable(&Retryable::Fatal));
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(defaults_and_classification)'`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Retry + per-attempt timeout policy for provider calls.
use std::time::Duration;

/// Retry + timeout policy.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub timeout: Duration,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_retries: 2, timeout: Duration::from_secs(60),
               base_backoff: Duration::from_millis(250), max_backoff: Duration::from_secs(30) }
    }
}
/// Coarse retry classification of an attempt failure.
pub enum Retryable { Transient, RateLimited { retry_after: Option<Duration> }, Fatal }
impl RetryPolicy {
    /// Whether a classified failure should be retried.
    pub fn is_retryable(&self, r: &Retryable) -> bool { !matches!(r, Retryable::Fatal) }
    /// Backoff for attempt `n` (0-based), capped, with deterministic jitter from `n`.
    pub fn backoff(&self, n: u32) -> Duration {
        let exp = self.base_backoff.saturating_mul(1u32 << n.min(16));
        exp.min(self.max_backoff)
    }
}
```

Add `mod retry;` to `lib.rs`. Map `ProviderError` → `Retryable` in a helper `classify(&ProviderError) -> Retryable` (Transport/timeout→Transient, Api{429}→RateLimited, Api{5xx}→Transient, else Fatal).

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(defaults_and_classification)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/retry.rs crates/ai-sdk/src/lib.rs
git commit -m "feat(ai-sdk): RetryPolicy + error classification"
```

### Task 6.3: `run()` loop + `AgentRun` (streaming, middleware, tools, stop, cancel)

**Files:**
- Modify: `crates/ai-sdk/src/agent.rs`
- Test: `crates/ai-sdk/tests/agent_loop.rs` (rewrite against the new API)

- [ ] **Step 1: Write the failing integration test**

Rewrite `tests/agent_loop.rs` to use a mock provider that returns one tool call then a text answer. Core test:

```rust
#[tokio::test]
async fn run_executes_tool_then_ends_and_streams() {
    let mut agent = Agent::new(())
        .provider(ScriptedProvider::two_step())   // step1: tool_call "echo"; step2: text "done"
        .model("scripted")
        .register_tool(EchoTool)
        .with_context(vec![Message::user("hi")]);

    let mut run = agent.run();
    let mut deltas = String::new();
    let mut saw_tool = false;
    let mut outcome = None;
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::MessageDelta(t) => deltas.push_str(&t),
            AgentEvent::ToolResult { .. } => saw_tool = true,
            AgentEvent::Done(o) => outcome = Some(o),
            _ => {}
        }
    }
    let out = outcome.unwrap();
    assert!(saw_tool);
    assert_eq!(out.text, "done");
    assert!(matches!(out.finish, Finish::EndTurn));
    assert_eq!(agent.messages().last_is_assistant(), true); // helper or check len grew
}
```

Add `ScriptedProvider`, `EchoTool` test fixtures in the test file.

- [ ] **Step 2: Run, verify it fails**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(run_executes_tool_then_ends)'`
Expected: FAIL — `Agent::run` missing.

- [ ] **Step 3: Implement `run()` + `AgentRun`**

In `agent.rs`, add `run(&mut self) -> AgentRun<'_>` returning a `futures::Stream` built with `async_stream::stream!`. The loop (mirror the old loop in the original `agent.rs:125-406`, adapted to the new types):

1. Clone `let middleware = self.middleware.clone();` (or take staging→Arc once).
2. If a skill loader is set and `self.skills` empty: `self.skills = loader.list(&self.ctx).await?` and inject `{name, description}` into the effective system prompt.
3. Build `AgentCx` by borrow-splitting `self` (`&self.ctx`, `&mut self.messages`, `&mut self.current_model`, `&mut self.system`, `&self.usage`, `&self.cancel`).
4. Run `on_start` for each middleware in order; first `Flow::Stop(msg)` → finish `Stopped` with msg.
5. Step loop:
   - `on_step` (each mw; first Stop halts).
   - Build `ChatRequest` from current model/system/tools(+built-in skill tools)/messages + apply `CacheStrategy` breakpoints + `cache_key`.
   - Resolve provider = `self.providers[current_model]`; call `provider.stream(req)` wrapped in retry+timeout (Phase 9 helper).
   - Stream `StreamEvent`s → emit `AgentEvent` deltas; accumulate text/reasoning/tool_calls; `tokio::select!` on cancel.
   - Append assistant message to `self.messages`; merge step `Usage` into `self.usage`; compute cost via `pricing`.
   - For each tool call: `on_tool_approve` (most-restrictive) → run tool or feed Deny text; built-in `tool_search`/`load_skill`/`search_skills` handled here; append tool results to `self.messages`; emit `ToolResult`.
   - Build `Step`, run `on_step_done` (each mw; first Stop halts), emit `StepEnd`.
   - If no tool calls → `Finish::EndTurn`; check `StopCond` (max steps/budget) → `Finish::Limit`.
6. Run `on_finish` for every mw whose `on_start` ran (reverse not required; registration order).
7. Set `Outcome.text` (final assistant text or Stop message) and yield `AgentEvent::Done(outcome)`.

Implement `AgentRun<'a>` wrapping the stream and an `IntoFuture`/`outcome()` that drains to the final `Done`. Cancellation = the stream's `Drop`.

```rust
/// A running agent: a stream of events; `.await`/`.outcome()` collects the final Outcome.
pub struct AgentRun<'a> { inner: futures::stream::BoxStream<'a, AgentEvent> }
impl<'a> futures::Stream for AgentRun<'a> { /* delegate */ }
impl<'a> AgentRun<'a> {
    /// Drain the stream and return the final outcome.
    pub async fn outcome(mut self) -> Result<Outcome, AgentError> { /* loop next(), return Done */ }
}
// impl IntoFuture for AgentRun -> outcome()
```

- [ ] **Step 4: Run, verify it passes**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(run_executes_tool_then_ends)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/agent.rs crates/ai-sdk/tests/agent_loop.rs
git commit -m "feat(ai-sdk): stateful run() loop + AgentRun stream/outcome"
```

### Task 6.4: Middleware drives the loop — stop, approval, model switch, conversation mutation

**Files:**
- Test: `crates/ai-sdk/tests/middleware.rs` (new)

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn on_step_stop_halts_with_message() {
    struct StopNow;
    #[async_trait::async_trait] impl AgentMiddleware<()> for StopNow {
        async fn on_step(&self, _cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> { Ok(Flow::stop("budget")) }
    }
    let mut agent = Agent::new(()).provider(ScriptedProvider::two_step()).model("scripted")
        .register_middleware(StopNow).with_context(vec![Message::user("hi")]);
    let out = agent.run().outcome().await.unwrap();
    assert!(matches!(out.finish, Finish::Stopped { .. }));
    assert_eq!(out.text, "budget");
}

#[tokio::test]
async fn on_start_can_prepend_conversation() {
    struct Loader;
    #[async_trait::async_trait] impl AgentMiddleware<()> for Loader {
        async fn on_start(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
            cx.messages_mut().splice(0..0, vec![Message::user("PRIOR")]); Ok(Flow::Continue)
        }
    }
    let mut agent = Agent::new(()).provider(ScriptedProvider::echo_history()).model("scripted")
        .register_middleware(Loader).with_context(vec![Message::user("NOW")]);
    let _ = agent.run().outcome().await.unwrap();
    assert_eq!(agent.messages()[0].text(), "PRIOR");
}

#[tokio::test]
async fn deny_tool_feeds_message_to_model_and_continues() { /* on_tool_approve -> Deny; assert ToolResult Denied; run still ends */ }
```

- [ ] **Step 2: Run, verify they fail**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(on_step_stop_halts) + test(on_start_can_prepend) + test(deny_tool_feeds)'`
Expected: FAIL until the loop honors these (it should from 6.3; fix any gaps).

- [ ] **Step 3: Fix loop gaps** until all pass (composition order, stop timing, deny vs stop).

- [ ] **Step 4: Run, verify pass.** Same command. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/tests/middleware.rs crates/ai-sdk/src/agent.rs
git commit -m "test(ai-sdk): middleware stop/approve/conversation-mutation in run loop"
```

---

## Phase 7: Skills wired into the loop + built-in tools

### Task 7.1: Inject skill manifests + built-in `search_skills`/`load_skill`

**Files:**
- Modify: `crates/ai-sdk/src/agent.rs` (loop init + built-in tool dispatch)
- Test: `crates/ai-sdk/tests/skills.rs` (new)

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn skills_listed_into_system_and_loaded_on_demand() {
    struct Mem;
    #[async_trait::async_trait] impl SkillLoader<()> for Mem {
        async fn list(&self, _c: &()) -> Result<Vec<Skill>, AgentError> {
            Ok(vec![Skill{ id:"pdf".into(), name:"PDF".into(), description:"read pdfs".into() }]) }
        async fn load(&self, _c: &(), id: &str) -> Result<SkillContent, AgentError> {
            assert_eq!(id, "pdf"); Ok(SkillContent{ body:"HOW TO PDF".into(), references: vec![] }) }
    }
    // ScriptedProvider: step1 emits a load_skill("pdf") tool call; step2 answers.
    let mut agent = Agent::new(()).provider(ScriptedProvider::loads_skill("pdf")).model("scripted")
        .skills(Mem).with_context(vec![Message::user("read this")]);
    let sys_seen = agent.debug_effective_system_contains("PDF"); // helper records injected system
    let out = agent.run().outcome().await.unwrap();
    assert!(out.text.contains("HOW TO PDF") || agent.messages().iter().any(|m| m.text().contains("HOW TO PDF")));
}
```

- [ ] **Step 2: Run, verify it fails.** Run: `cargo nextest run -p stakit-ai-sdk -E 'test(skills_listed_into_system)'`. Expected: FAIL.

- [ ] **Step 3: Implement** in the loop init: call `loader.list(&ctx)`, cache in `self.skills`, append a manifest block (`- {name}: {description}` lines) to the effective system prompt; when a loader is present, register built-in tools `search_skills(query)` (substring/full-text over `self.skills`) and `load_skill(id)` (calls `loader.load`, returns body as the tool result). Dispatch these in the tool-call branch before user tools.

- [ ] **Step 4: Run, verify pass.** Same command. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/agent.rs crates/ai-sdk/tests/skills.rs
git commit -m "feat(ai-sdk): inject skill manifests + built-in search_skills/load_skill"
```

---

## Phase 8: Caching

### Task 8.1: `CacheStrategy::Auto` breakpoints (Claude) + `prompt_cache_key` (OpenAI)

**Files:**
- Modify: `crates/ai-sdk/src/cache.rs` (a `plan(&self, req) -> CachePlan` helper), `crates/ai-sdk/src/provider/claude.rs`, `crates/ai-sdk/src/provider/openai.rs`
- Test: `crates/ai-sdk/tests/cache.rs` (new)

- [ ] **Step 1: Write the failing tests (offline, snapshot the request body)**

```rust
#[test]
fn claude_auto_places_breakpoints_after_tools_and_system_and_rolling() {
    let req = sample_request_with_tools_system_and_two_turns();
    let body = claude_build_body(&req, CacheStrategy::Auto, Some("sess-1"));
    let bps = count_cache_control_breakpoints(&body);
    assert!(bps >= 2 && bps <= 4);
    assert!(breakpoint_after_tools(&body));
    assert!(breakpoint_after_system(&body));
}

#[test]
fn openai_auto_sets_prompt_cache_key_from_session() {
    let req = sample_request();
    let body = openai_build_body(&req, CacheStrategy::Auto, Some("sess-1"));
    assert_eq!(body["prompt_cache_key"], "sess-1");
}

#[test]
fn shared_prefix_is_byte_identical_across_users() {
    let a = claude_build_body(&req_for_user("A"), CacheStrategy::Auto, Some("A"));
    let b = claude_build_body(&req_for_user("B"), CacheStrategy::Auto, Some("B"));
    assert_eq!(prefix_up_to_last_shared_breakpoint(&a), prefix_up_to_last_shared_breakpoint(&b));
}
```

- [ ] **Step 2: Run, verify they fail.** Run: `cargo nextest run -p stakit-ai-sdk -E 'test(claude_auto_places) + test(openai_auto_sets) + test(shared_prefix_is_byte)'`. Expected: FAIL.

- [ ] **Step 3: Implement** the breakpoint planner in `cache.rs` (return target indices for tools/system/rolling-last-turn) and wire `claude.rs` (`cache_control` on those blocks, ≤4) and `openai.rs` (set `prompt_cache_key`). The agent passes `cache_key(&ctx)` into the build.

- [ ] **Step 4: Run, verify pass.** Same command. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/cache.rs crates/ai-sdk/src/provider/
git commit -m "feat(ai-sdk): scale-correct CacheStrategy::Auto (claude breakpoints, openai cache key)"
```

---

## Phase 9: Retry + timeout in the loop

### Task 9.1: Wrap provider calls with retry + per-attempt timeout

**Files:**
- Modify: `crates/ai-sdk/src/agent.rs` (a `call_provider_with_retry` helper)
- Test: `crates/ai-sdk/tests/retry.rs` (new)

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test(start_paused = true)]
async fn retries_transient_then_succeeds() {
    let provider = FlakyProvider::fail_n_times(2); // 2 transient errors then ok
    let mut agent = Agent::new(()).provider(provider).model("flaky").with_retries(3)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().outcome().await.unwrap();
    assert!(matches!(out.finish, Finish::EndTurn));
}

#[tokio::test(start_paused = true)]
async fn hung_provider_trips_timeout_and_retries() {
    let provider = HangingProvider; // never responds
    let mut agent = Agent::new(()).provider(provider).model("hang")
        .with_retries(1).with_timeout(std::time::Duration::from_secs(5))
        .with_context(vec![Message::user("hi")]);
    let res = agent.run().outcome().await;
    assert!(res.is_err() || matches!(res.unwrap().finish, Finish::Limit(_)));
}

#[tokio::test(start_paused = true)]
async fn fatal_4xx_is_not_retried() { /* FlakyProvider::fatal(); assert single attempt */ }
```

- [ ] **Step 2: Run, verify they fail.** Run: `cargo nextest run -p stakit-ai-sdk -E 'test(retries_transient) + test(hung_provider_trips) + test(fatal_4xx)'`. Expected: FAIL.

- [ ] **Step 3: Implement** `call_provider_with_retry`: loop up to `max_retries`, wrap the `stream()` setup + first-byte in `tokio::time::timeout`, classify failures via `retry::classify`, sleep `backoff(n)` (honor `Retry-After`), and **only retry before the first token is emitted** (track a `first_token_seen` flag; once true, surface the error).

- [ ] **Step 4: Run, verify pass.** Same command. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ai-sdk/src/agent.rs crates/ai-sdk/tests/retry.rs
git commit -m "feat(ai-sdk): retry + per-attempt timeout around provider calls"
```

---

## Phase 10: Removals + `lib.rs` flip

### Task 10.1: Delete dead modules and old hooks; flip exports

**Files:**
- Delete: `crates/ai-sdk/src/context.rs`
- Modify: `crates/ai-sdk/src/lib.rs`
- Modify: `crates/ai-sdk/tests/extensibility.rs`, `crates/ai-sdk/tests/e2e.rs`

- [ ] **Step 1: Delete `context.rs`**

```bash
git rm crates/ai-sdk/src/context.rs
```

- [ ] **Step 2: Update `lib.rs`**

Remove `mod context;` and the `ContextLoader/FsContextLoader/LoadedContext` re-exports and the old `Permission`/`LoopEvent`/`FinishReason`/`FsSkillLoader`/`SkillManifest` re-exports. Add the new public surface:

```rust
mod control;
mod middleware;
mod retry;

pub use agent::{Agent, AgentRun};
pub use control::{Approval, Flow};
pub use cx::AgentCx;
pub use error::{AgentError, ProviderError, ToolError};
pub use loop_event::{AgentEvent, Finish, Outcome, PendingToolCall, Step, StopCond, ToolCallRecord, ToolOutcome};
pub use message::{AssistantContent, Image, Message, SystemPrompt, Thinking, UserContent};
pub use middleware::AgentMiddleware;
pub use provider::{ChatRequest, ChatResponse, EventStream, Provider, StopReason, StreamEvent, ThinkingConfig, ToolChoice, ToolDef};
pub use retry::RetryPolicy;
pub use skill::{Skill, SkillContent, SkillLoader};
pub use tool::{Tool, ToolDyn, ToolSet, TypedTool};
pub use usage::{ModelPrice, Pricing, Usage};
pub use cache::{CacheStrategy, CacheTarget, CacheTtl};
pub use cancel::CancelToken;
pub use mcp::{McpConfig, McpServer, McpTool, McpToolSet, McpTransport};
pub use stakit_ai_sdk_derive::tool;
#[cfg(feature = "claude")] pub use provider::claude::{ClaudeClient, ClaudeModel};
#[cfg(feature = "openai")] pub use provider::openai::{OpenAiClient, OpenAiModel};
```

- [ ] **Step 3: Update `extensibility.rs` + `e2e.rs`** to the new API (replace `ContextLoader`/`Permission`/`AgentBuilder` usages with middleware + the stateful agent; replace `LoopEvent`→`AgentEvent`).

- [ ] **Step 4: Build + full test**

Run: `cargo build -p stakit-ai-sdk --all-features && cargo nextest run -p stakit-ai-sdk`
Expected: builds clean; all tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/ai-sdk
git commit -m "refactor(ai-sdk): remove ContextLoader/old hooks/FsSkillLoader; flip lib exports"
```

---

## Phase 11: Acceptance + gate

### Task 11.1: Live cache tests per provider (feature-gated)

**Files:**
- Create: `crates/ai-sdk/tests/cache_live.rs`

- [ ] **Step 1: Write the gated tests**

```rust
#![cfg(feature = "live-tests")]
// Reads ANTHROPIC_API_KEY / OPENAI_API_KEY; skips (returns Ok) if absent.

#[tokio::test]
async fn claude_second_call_reads_cache() {
    let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else { return; };
    // build agent with a large stable system+tools prefix; run twice on the same session
    // assert run #2 Outcome/usage has cache_read_tokens > 0
}

#[tokio::test]
async fn openai_second_call_reports_cached_tokens() {
    let Ok(key) = std::env::var("OPENAI_API_KEY") else { return; };
    // same, assert openai cached-token field > 0 on run #2
}
```

Add a `live-tests` feature to `crates/ai-sdk/Cargo.toml`.

- [ ] **Step 2: Run (no keys → skips)**

Run: `cargo nextest run -p stakit-ai-sdk --features live-tests -E 'test(claude_second_call) + test(openai_second_call)'`
Expected: PASS (skipped) without keys; PASS with `cache_read_tokens > 0` when keys present.

- [ ] **Step 3: Commit**

```bash
git add crates/ai-sdk/tests/cache_live.rs crates/ai-sdk/Cargo.toml
git commit -m "test(ai-sdk): feature-gated live prompt-cache tests per provider"
```

### Task 11.2: Provider-switch test

**Files:**
- Create: `crates/ai-sdk/tests/provider_switch.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn set_model_switches_provider_mid_run() {
    struct Switcher;
    #[async_trait::async_trait] impl AgentMiddleware<()> for Switcher {
        async fn on_step(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
            if cx.index() >= 1 { cx.set_model("b"); } Ok(Flow::Continue)
        }
    }
    let mut agent = Agent::new(())
        .provider(TaggedProvider::new("a"))         // tags its output with "a"
        .register_provider(TaggedProvider::new("b"))
        .model("a").register_middleware(Switcher)
        .with_context(vec![Message::user("hi")]);
    let out = agent.run().outcome().await.unwrap();
    assert!(out.text.contains("b"));  // step 2 used provider b
}
```

- [ ] **Step 2: Run, verify fail→pass; fix loop if `set_model` not re-resolved per step.**

Run: `cargo nextest run -p stakit-ai-sdk -E 'test(set_model_switches_provider)'`

- [ ] **Step 3: Commit**

```bash
git add crates/ai-sdk/tests/provider_switch.rs
git commit -m "test(ai-sdk): set_model switches active provider mid-run"
```

### Task 11.3: Full quality gate + examples

**Files:**
- Modify: `crates/ai-sdk/examples/*.rs` (`chat`, `weather_agent`) to the new API
- Modify: `crates/ai-sdk/tests/tool_macro.rs` if it referenced old cx types

- [ ] **Step 1: Update examples** to `Agent::new(ctx).provider(...).register_tool(...).with_context(...).run()`.

- [ ] **Step 2: Run the gate**

Run: `./code-check.sh`
Expected: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build`, `cargo nextest run`, doctests all pass.

- [ ] **Step 3: Commit**

```bash
git add -A crates/ai-sdk
git commit -m "chore(ai-sdk): update examples + pass full quality gate"
```

---

## Phase 12: `LLM` — single-call structured extraction

A thin one-shot helper over a `Provider` for structured output into a `stakit-model` type (no agent loop, no middleware, single turn).

### Task 12.1: `LLM<P>` builder with `extract::<T>()` and `text()`

**Files:** create `crates/ai-sdk/src/llm.rs`; modify `crates/ai-sdk/src/lib.rs`.

Target usage:
```rust
#[derive(serde::Deserialize, stakit_model::Model, stakit_model::JsonSchema)]
struct User { name: String, age: u32 }

let user: User = LLM::new(OpenAiProvider::from_env()?.model("gpt-5"))
    .system("Extract the user from the text.")
    .user("Bob is 30")
    .extract::<User>().await?;          // structured output → typed value

let answer: String = LLM::new(provider).system("..").user("hi").text().await?;  // plain text
```

- [ ] **Step 1: failing test** in `llm.rs`: a `MockProvider` whose `complete` returns a `ChatResponse` containing a `ToolUse` block with `{"name":1,...}`-style JSON; assert `extract::<T>()` returns the typed struct; a second mock returning text asserts `text()`.

- [ ] **Step 2: run, verify fail.** `cargo nextest run -p stakit-ai-sdk --lib -E 'test(llm_)'`.

- [ ] **Step 3: implement.** `LLM<P: Provider>` builder: `new(provider)`, `system(impl Into<String>)`, `model(impl Into<String>)` (default `provider.model_id()`), `user(impl Into<Arc<str>>)`, `max_tokens(u32)`, `temperature(f32)`.
  - `extract::<T: stakit_model::JsonSchema + serde::de::DeserializeOwned>(self) -> Result<T, AgentError>`: build a `ChatRequest` with a single `ToolDef { name: "extract", description: "Return the structured result.", parameters: T::schema(), .. }`, `tool_choice = ToolChoice::Tool("extract")` (forced), system + one user message; call `provider.complete(req).await?`; find the `ToolUse` block; `serde_json::from_value::<T>(args)` (map errors to `AgentError`).
  - `text(self) -> Result<String, AgentError>`: no tools; `complete`; concatenate `AssistantContent::Text` blocks.

- [ ] **Step 4: run, verify pass.** Same command. Then full `cargo nextest run -p stakit-ai-sdk --lib --tests --all-features` + `cargo clippy -p stakit-ai-sdk --lib --tests --all-features -- -D warnings`.

- [ ] **Step 5:** do NOT commit (no git).

**Context:** single-turn structured output works on both providers via a forced tool call whose input schema is the model's `JsonSchema::schema()`. No streaming, no middleware. `LLM` is generic over `P: Provider` (used directly with `ClaudeModel`/`OpenAiModel`).

---

## Self-review notes (already applied)

- **Spec coverage:** §3–§8 → Phases 4–7; §11 providers → Phases 2,11.2; §12 cache → Phase 8 + 11.1; §13 retry → Phases 6.2,9; §10 skills → Phases 5,7; §5 removals → Phase 10; §15 memory model = inherent (stateful agent + middleware load/save, no new task); §14 no-runner = a removal/non-goal, nothing to build.
- **Type consistency:** `AgentCx`, `AgentMiddleware`, `Flow`, `Approval`, `AgentEvent`, `Outcome`, `Finish`, `Step`, `ToolOutcome`, `PendingToolCall`, `Skill`, `SkillLoader`, `Provider` (dyn), `Agent<Ctx>`, `AgentRun`, `RetryPolicy`, `Image`, `Message` used consistently across phases.
- **Open items deferred to execution:** exact borrow-split shape in `AgentCx`/`run()` (Task 6.1 note), `run_with(msg)` sugar (spec §20) — not required for acceptance.
