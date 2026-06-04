# Agent refactor — design

- **Date:** 2026-06-04
- **Crate:** `stakit-ai-sdk` (`crates/ai-sdk`)
- **Status:** approved for planning
- **Type:** green-field refactor — old/non-working abstractions are removed, not preserved.

## 1. Goals

1. One call does everything: `agent.run(ctx, messages)` runs the full agentic
   loop (many steps until the model stops / a limit hits / a middleware stops /
   error). The host never writes a step loop.
2. **One** composable async trait — `AgentMiddleware` — for context load/save,
   tool approval, stop, model/system switching, observation. Memories, billing,
   permissions, persistence are all just middlewares.
3. **Context is not a trait.** History is the run's internal `Vec<Message>`,
   loaded/saved by a middleware (db, file, anywhere). No `ContextLoader`.
4. **Cheap, stateless `Agent`** — build it anywhere (action, cron, sub-agent).
   Provider clients are `Arc` inside, so reuse is cheap; the agent holds no
   per-user/per-run state.
5. **Runtime provider + model switching** — register N providers; a middleware
   picks model+provider per run via `set_model`.
6. **Automatic, scale-correct prompt caching** for both OpenAI and Claude.
7. **Streaming** output (`AgentEvent`) so the host can pipe deltas to the user.
8. Idiomatic Rust: immutable shared config, per-run owned state, borrowed reads,
   `Arc<str>`-backed message content, `tracing` spans.

## 2. Non-goals

- **Background-run orchestration is OUT of scope (app-level).** The SDK provides
  `agent.run()` and nothing past it. Tracking running tasks, get-or-start,
  resume-to-live, killing finished runs — that is a **generic task registry the
  host owns** (see §15). The SDK has no runner, no run-handle registry, no
  `Session`/`Conversation` type.
- Multi-provider failover / load balancing.
- Structured/typed final-output schema (future; §18).
- Tool-output truncation policy (future; §18).
- Provider-side conversation storage (e.g. OpenAI Responses `previous_response_id`).
  We always resend from our own history; the provider holds nothing for us.

## 3. Mental model — three separate things

Never conflate config, execution state, and storage.

```
┌──────────────────────────────────────────────────────────┐
│ 1. AGENT — config TEMPLATE. Immutable. Cheap. Shareable.  │
│    provider registry · tools · middleware                 │
│    DEFAULT model · DEFAULT system · cache/retry config    │  ← built once, NEVER mutated
└──────────────────────────────────────────────────────────┘
      │  agent.run(&self, ctx, msgs)   ← &self: cannot mutate the agent
      ▼  (creates a fresh Run per call)
┌──────────────────────────────────────────────────────────┐
│ 2. RUN — execution state. Per-call. OWNED. Not shared.    │
│    history: Vec<Message>   ← loaded from DB by middleware  │
│    model: chosen this run  ← set_model overrides default   │  ← lives only during the call
│    usage · cost · cancel                                  │
└──────────────────────────────────────────────────────────┘
      │  middleware on_step_done → db.save(session, history)
      ▼
┌──────────────────────────────────────────────────────────┐
│ 3. DATABASE — durable. ALL users' histories live here.    │
│    "user-1" → [messages…]   "user-2" → [messages…]  …     │  ← contexts at rest, host-owned
└──────────────────────────────────────────────────────────┘
```

Consequences:
- `.model("gpt-5")` stores an **immutable default** on the agent. `set_model`
  inside a run mutates **that run's** state, never the shared agent. Two
  concurrent runs on different models never interfere.
- A history lives in **RAM only while its run is executing**; at rest it is in
  the **db**. 10k idle users = 10k db records, ~0 RAM (see §16).

## 4. SDK scope vs app scope (the boundary)

| Concern | Who |
| --- | --- |
| `Agent`, `agent.run(ctx, msgs)`, the step loop | **SDK** |
| `AgentMiddleware` (load/save/stop/approve/switch) | **SDK** |
| `Tool`, `SkillSource`, `Provider`, `Message`, caching, retry, usage | **SDK** |
| Tracking running tasks, get-or-start, dedup, kill-on-finish | **App** (§15) |
| Where histories are stored (db/file) | **App** (via a middleware) |
| Streaming transport to the end user (SSE/WS) | **App** (consume `AgentEvent`) |

## 5. What changes vs today

| Concept | Today | Decision |
| --- | --- | --- |
| `Agent<P, Ctx>` (static provider `P`) | exists | **drop `P`** → `Agent<Ctx>` + dyn provider registry |
| `agent.run() -> Stream<LoopEvent>` | exists | keep streaming; rename `LoopEvent` → `AgentEvent`; add `.outcome()` + `IntoFuture` |
| `Tool<Ctx>` + registry + `#[tool]` | exists | keep |
| `SkillLoader { list, load }` + `FsSkillLoader` | exists | reshape → `SkillSource`, slim `Skill { id, name, description }` |
| `ContextLoader` / `FsContextLoader` / `LoadedContext` | exists | **remove** — context is a middleware now |
| hooks `can_use_tool` / `on_ask` / `prepare_step` | exist | **remove** — folded into `AgentMiddleware` |
| `Permission` enum | exists | reshape → `Approval` (Allow / Deny / Stop) |
| `CacheStrategy` / `CacheTtl` | exists | keep; `Auto` scale-correct (§13) |
| `Usage` / `Pricing` | exists | keep; accumulated totals |
| `Message` | exists | make content `Arc<str>`/`Bytes`; images = `Image { Url | FileId | Base64 }` |

### Removal list
- `context.rs` entirely (`ContextLoader`, `FsContextLoader`, `LoadedContext`).
- The three builder hook closures + their type aliases in `agent.rs`.
- `Permission` (→ `Approval`).
- The static provider generic `P` on `Agent`.
- Rename `LoopEvent`/`FinishReason` → `AgentEvent`/`Finish`.

## 6. Public surface

```rust
let agent = Agent::<ReqCtx>::new()                 // cheap; build in an action / cron / sub-agent
    .register_provider(OpenAiProvider::new(key).model("gpt-5"))      // dyn registry, keyed by model id
    .register_provider(AnthropicProvider::new(key).model("claude-opus"))
    .model("gpt-5")                                // default model+provider
    .system("You are helpful.")                    // default system
    .tool(Search)
    .middleware(SessionStore)                      // context load/save (db) — see §9
    .middleware(Billing)                           // stop on budget, switch model — see §8
    .cache(CacheStrategy::Auto)                    // §13
    .cache_key(|c: &ReqCtx| Some(c.session.clone()))  // prompt-cache key (session id)
    .with_retries(5)
    .with_timeout(Duration::from_secs(30))
    .build();

// run — the full agentic loop, many steps until stop/limit/error:
let out = agent.run(ctx, vec![Message::user("hi")]).await?;   // IntoFuture → Outcome
println!("{}", out.text);

// or stream deltas to the user:
let mut run = agent.run(ctx, vec![Message::user("hi")]);
while let Some(ev) = run.next().await { /* pipe AgentEvent over SSE/WS */ }
```

`run` signature:

```rust
pub fn run(&self, ctx: Ctx, new_messages: Vec<Message>) -> AgentRun;
//                  ^owned (so a detached/cron run can outlive a request)
//                       ^the new turn only (or empty to pure-resume — middleware rehydrates)
```

- `ctx: Ctx` is **owned**, moved into the run. Tools/middleware read it via
  `cx.ctx()` (`&Ctx`). It carries the host's handles (db pool, user id, session,
  per-user model choice). May wrap a router `Cx<App, Req>`.
- `new_messages` seeds the run's internal `Vec<Message>`; a `SessionStore`-style
  middleware prepends prior history in `on_start` (§9).
- `Agent::new()` is **cheap**: it assembles handle lists; provider HTTP clients
  are `Arc` inside, so registering them is a pointer bump. Recreate freely.

### `AgentRun`, events, outcome, cancellation

```rust
pub struct AgentRun { /* impl Stream<Item = AgentEvent> + IntoFuture<Output = Result<Outcome, AgentError>> */ }
impl AgentRun {
    pub async fn outcome(self) -> Result<Outcome, AgentError>;  // drain → final Done
}

pub enum AgentEvent {
    StepStart  { index: u32 },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCall   { id: String, name: String, args: Value },               // compacted args
    ToolResult { id: String, name: String, result: ToolOutcome },
    StepEnd    { index: u32, text: String, reasoning: Option<String>, usage: Usage, cost: Option<f64> },
    Done(Outcome),
}

pub struct Outcome {
    pub text: String,            // final text — OR a middleware Stop message
    pub messages: Vec<Message>,  // full final history (for hosts that thread it themselves)
    pub usage: Usage,            // accumulated
    pub cost: Option<f64>,       // accumulated USD
    pub steps: u32,
    pub finish: Finish,
}
pub enum Finish { EndTurn, Limit(StopCond), Stopped { message: Option<String> }, Cancelled }
```

- **Granularity:** stream deltas token-by-token **and** include assembled
  `text`/`reasoning` in `StepEnd` (consumers that don't accumulate still get
  whole messages).
- **Cancellation = drop the `AgentRun`.** When the host's task is aborted/dropped
  (e.g. its registry kills it), the run future drops and stops at the next await
  (provider stream / tool boundary), aborting the in-flight provider request.
  This is the idiomatic async cancel; the SDK needs no run-handle registry. (A
  `CancelToken` may be carried in `ctx` for cooperative tool cancellation.)

## 7. `AgentMiddleware` — the one extension trait

Stored as `Vec<Box<dyn AgentMiddleware<Ctx>>>` → dyn-safe → use `#[async_trait]`
(hooks fire ~once per step; the box is negligible). `Tool`/`Provider` keep their
native styles.

```rust
#[async_trait]
pub trait AgentMiddleware<Ctx>: Send + Sync + 'static {
    /// Before the first model call. Load history / inject memory / guidance.
    async fn on_start(&self, cx: &mut AgentCx<Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// Before each model call. Check budget, switch model/system, drain queued input, compact.
    async fn on_step(&self, cx: &mut AgentCx<Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// After each step resolves (full Step detail). Persist / observe.
    async fn on_step_done(&self, cx: &mut AgentCx<Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// Gate every tool call.
    async fn on_tool_approve(&self, cx: &AgentCx<Ctx>, call: &PendingToolCall) -> Result<Approval, AgentError> { Ok(Approval::Allow) }
    /// After the loop ends (any reason). Persist final / cleanup. Terminal.
    async fn on_finish(&self, cx: &AgentCx<Ctx>) -> Result<(), AgentError> { Ok(()) }
}

pub enum Flow { Continue, Stop(String) }   // Err(AgentError) = error stop; Stop = graceful w/ final text
pub enum Approval { Allow, Deny { message: String }, Stop { message: Option<String> } }
```

`Deny` (tool blocked, model sees the reason, loop continues) ≠ `Stop` (agent
halts).

### `AgentCx<Ctx>` — what hooks get

```rust
impl AgentCx<Ctx> {
    fn ctx(&self) -> &Ctx;                  // host handles (db/user/session) — read-only
    fn history(&self) -> &[Message];        // borrow — zero copy
    fn history_mut(&mut self) -> &mut Vec<Message>;  // inject / prepend / compact / replace
    fn usage(&self) -> &Usage;              // accumulated
    fn cost(&self) -> Option<f64>;          // accumulated USD
    fn index(&self) -> u32;                 // current step
    fn step(&self) -> Option<&Step>;        // last completed step (in on_step_done)
    fn set_model(&mut self, id: impl Into<String>);   // per-run override → resolves provider from registry
    fn set_system(&mut self, text: impl Into<String>);// per-run system override
    fn state(&self) -> &Extensions;         // per-run typemap (state across hooks/steps)
    fn state_mut(&mut self) -> &mut Extensions;
}

pub struct Step { pub index: u32, pub reasoning: Option<String>, pub text: String,
                  pub tool_calls: Vec<ToolCallRecord>, pub stop: StopReason }
pub struct ToolCallRecord { pub id: String, pub name: String, pub args: Value,
                            pub approval: Approval, pub result: ToolOutcome, pub elapsed: Duration }
pub enum ToolOutcome { Ok(Value), Denied { message: String }, Error(String) }
```

- **Per-run state** lives in the run (history, model, usage, `Extensions`),
  reached via `&mut AgentCx`. A middleware `&self` is **shared across all runs**,
  so it must hold only shared handles (a `db: Pool`), never per-run fields — per-
  run state goes in `cx.state_mut()` (a typemap), and per-run data comes from
  `cx.ctx()`.
- **Per-step model/system switch**: `set_model`/`set_system` mutate the run, not
  the agent. A model switch resolves a different registered provider (§12) and
  resets that provider's cache (expected).

### Composition (locked)
- `on_start` / `on_step` / `on_step_done`: registration order; first `Flow::Stop`
  halts and skips the rest of that hook.
- `on_tool_approve`: registration order; first non-`Allow` wins (Stop > Deny > Allow).
- `on_finish`: registration order; runs for every middleware whose `on_start`
  ran, even on stop/error.
- Stop takes effect at the **next hook boundary** (no mid-stream kill; drop =
  hard cancel).

## 8. Context = middleware (no trait)

History is the run's internal `Vec<Message>`, reached via `cx.history()` /
`cx.history_mut()`. Loading and saving are **middleware**, not a trait — db,
file, anywhere. Example:

```rust
struct SessionStore;
#[async_trait]
impl AgentMiddleware<ReqCtx> for SessionStore {
    async fn on_start(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        let prior = cx.ctx().db.load_history(&cx.ctx().session).await.map_err(AgentError::context)?;
        cx.history_mut().splice(0..0, prior);     // prepend prior before the new turn
        Ok(Flow::Continue)
    }
    async fn on_step_done(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        cx.ctx().db.save_history(&cx.ctx().session, cx.history()).await.map_err(AgentError::context)?;
        Ok(Flow::Continue)                        // checkpoint — consistent at step boundary
    }
}

// per-user model/system come from ctx, applied in a middleware:
struct PerUser;
#[async_trait]
impl AgentMiddleware<ReqCtx> for PerUser {
    async fn on_step(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        cx.set_model(cx.ctx().model.clone());     // user-1 "gpt-5", user-2 "claude-opus"
        if cx.cost().unwrap_or(0.0) >= cx.ctx().budget { return Ok(Flow::Stop("Out of budget.".into())); }
        Ok(Flow::Continue)
    }
}
```

**Checkpoint only at `on_step_done`** — at a step boundary every `tool_use` has
its `tool_result`, so the persisted history is always consistent. Crash mid-step
⇒ resume from the last clean checkpoint, losing only the partial step.

Queued mid-run input: a middleware drains a channel from `cx.ctx()` in `on_step`
and `cx.history_mut().push(...)`. No dedicated input API.

## 9. Tools

`Tool<Ctx>` (typed, `#[tool]` macro) unchanged; stored erased as
`Box<dyn ToolDyn<Ctx>>`. Tool bodies get `&AgentCx<Ctx>` and read host handles via
`cx.ctx()`. Deferred tools + built-in `tool_search` retained.

## 10. Skills — `SkillSource`

Progressive disclosure (matches Claude Code / OpenAI): inject `{name,
description}` at start (cheap); fetch the body on demand via a built-in tool.

```rust
pub struct Skill { pub id: String, pub name: String, pub description: String }  // NO body

#[async_trait]
pub trait SkillSource<Ctx>: Send + Sync {
    async fn list(&self, cx: &AgentCx<Ctx>) -> Result<Vec<Skill>, AgentError>;
    async fn load(&self, cx: &AgentCx<Ctx>, id: &str) -> Result<SkillContent, AgentError>;
}
pub struct SkillContent { pub body: String, pub references: Vec<String> }
```

At run start `list()` injects manifests into the system prompt; built-in
`search_skills(query)` (full-text over the list) and `load_skill(id)` are
auto-registered when a source is present. `FsSkillLoader` is reshaped into an
`FsSkillSource` reference impl. Source can be db, fs, anywhere.

## 11. Providers — dyn registry, runtime switch, neutral message

`Agent<Ctx>` holds `IndexMap<String, Arc<dyn Provider>>` keyed by model id (`P`
generic dropped). `Provider` is erased (erase associated `Raw`, return
`BoxFuture`). Dyn dispatch is free next to the network HTTP boundary.

- `.register_provider(p)` registers (keyed by `p.model_id()`).
- `.model(id)` = default. `cx.set_model(id)` = per-run/per-step override → resolves
  model **and** provider.
- **Cross-provider switch mid-conversation** (Claude↔OpenAI): history is stored in
  a **neutral `Message`**; each provider adapter translates at send time,
  down-converting what its backend can't represent (e.g. drop Claude thinking
  signatures when sending to OpenAI). Switching resets that provider's cache; new
  tool-call ids issue going forward; usage accrues per provider into one `Usage`.
- **Single shared API key, 10k users** → one agent, key inside the provider,
  registered once. The bottleneck is provider **rate limits**, not the agent —
  bound concurrency host-side (semaphore) + retry 429 (§14).

### Message + images

```rust
pub struct Message { /* role + content; text as Arc<str> for cheap clone */ }
pub enum Image {
    Url(Arc<str>),                                // DEFAULT — cheap string, RAM-friendly, re-send-cheap
    FileId(Arc<str>),                             // uploaded once (provider Files API) — best for repeats
    Base64 { media_type: Arc<str>, data: Bytes }, // only when no URL; big; Bytes = cheap clone
}
```

Prefer `Url`/`FileId`: base64 bloats every request, every cache-write, and RAM.
History stays small (URLs/ids, not megabytes of base64). Content is `Arc<str>`/
`Bytes` so cloning a `Message` is a pointer bump, not a deep copy.

## 12. Caching

Two orthogonal concerns: **cost** (prompt-cache breakpoints) and the fact that
**we always resend from our own history** (no provider-side state).

Provider facts (verified):
- **Claude:** `cache_control:{type:"ephemeral"}` breakpoints, max 4. Prefix-based,
  **exact content match — resend the full prefix every turn**. Min 1024/2048 tok.
  TTL 5 min / 1 h beta. Write 1.25×/2× input; read **0.1×**. Order: tools → system
  → messages.
- **OpenAI:** automatic, ≥1024 tok, prefix match, routed by org + optional
  `prompt_cache_key`. ~50–75% off cached input. Also resend-based.

`CacheStrategy::Auto`:
- order content **stable → variable**: `tools → system → skills-manifest →
  shared-instructions → [per-user memory] → conversation`.
- **Claude:** breakpoint after tools, after system+skills (global shared prefix),
  rolling breakpoint on the previous turn's last message. ≤ 4.
- **OpenAI:** set `prompt_cache_key` from `cache_key(ctx)`; rely on auto prefix
  caching with the ordering above.

**Stability rule:** anything injected **before** a breakpoint must be byte-stable
across turns or it busts the cache. Shared static (tools/system/skills) →
cached across all users; per-user memory that is stable within a conversation →
between global and rolling breakpoints; volatile per-turn injection → after all
breakpoints.

Cache expiry is **harmless** (we always resend full content → provider just
recomputes, costing full input that turn). `cache_key` comes from a builder
closure `.cache_key(|ctx| Option<Arc<str>>)`.

## 13. Retry & timeout

Core loop behavior (wraps each provider call); cannot be a middleware.

```rust
.with_retries(5)                       // attempts on transient errors (default 2)
.with_timeout(Duration::from_secs(30)) // PER-ATTEMPT; a hung connection trips it
.with_retry_policy(RetryPolicy { .. }) // advanced; default backoff = exponential + full jitter
```

Retryable: `Transport`, per-attempt timeout, `Api{429}` (honor `Retry-After`),
`Api{5xx}`. Not: `Api{4xx≠429}`, `InvalidArgument`, `Decode`, `ToolError`,
`Cancelled`. **Streaming caveat:** retry only before the first token; a
mid-stream failure surfaces as `Finish::*`/error (no duplicated output).

## 14. Background, resume, cron — APP-LEVEL (out of SDK)

The SDK ends at `agent.run()`. Everything below is a **pattern the host
implements** with its own generic task registry; the SDK has no part in it.

- **Generic runner registry** (host-owned, agent-agnostic): a
  `DashMap<Id, Task>` with **get-or-start** (dedup → one run per session, avoids
  history corruption) that **self-clears on finish** (task ends → removed). **No
  idle entries** — an idle session holds nothing in RAM; its history is in db.
- **Start detached / cron:** `runner.get_or_start(id, || agent.run(ctx, msgs))`,
  driven on a spawned task.
- **Resume after crash / cron wake:** `agent.run(ctx, vec![])` — empty input →
  the `SessionStore` middleware rehydrates history from db → the agent continues.
- **Live re-attach:** the host forwards `AgentEvent`s to its own broadcast keyed
  by id; a reconnecting client subscribes. (Optional; host's choice of transport.)
- **Stop:** the host aborts/drops the task → the `AgentRun` drops → run stops.

This keeps the SDK free of run-handles, registries, sessions, and "runner"
machinery. The host composes background behavior from `agent.run()` + a generic,
reusable task registry that can run anything.

## 15. Concurrency & memory model (10k users / one process)

- Agent config is immutable; `run(&self, …)` cannot mutate it → safe to share or
  recreate. Per-run state is owned by the run.
- A history is in RAM **only while its run executes**; loaded by middleware on
  start, saved each step, dropped at end. **Idle sessions = 0 RAM** (db only).
- With one API key, concurrent runs are **rate-limit-bound** (a few hundred, not
  10k). ~100 KB/history × hundreds = tens of MB. The db holds the 9,000+ idle.
- Bound concurrency host-side with a `tokio::Semaphore` around `run()`.
- Zero-copy: `cx.history()` borrows `&[Message]`; the agent appends owned new
  messages; `Message` content is `Arc<str>`/`Bytes`. The only unavoidable
  allocation is the JSON request body at send (caching requires sending it).

## 16. Tracing

1. **`tracing` spans:** span per run (tagged with `cache_key`/session), child span
   per step / tool / provider call; record accumulated usage+cost on close.
2. **`AgentEvent` stream:** live trace to the user.
3. **`on_step_done` middleware:** durable custom trace (write `Step` to db).

## 17. Dependencies

- Add `async-trait` to `[workspace.dependencies]` (pin newest — check crates.io),
  pull into the crate.
- `tracing` already in the workspace (`0.1.44`); add to the crate for spans.
- `Extensions` (per-run typemap, §7) hand-rolled
  (`HashMap<TypeId, Box<dyn Any + Send + Sync>>`) — no new dependency.
- `indexmap` (already present) for the ordered provider registry.

## 18. Deferred (future passes)

- Retry/backoff already in-scope (§13).
- Structured/typed final-output schema.
- Tool-output truncation policy.
- Live re-attach helpers (the SDK stays out; a host example may show a registry).

## 19. Testing strategy

Acceptance goals: (1) **strict cache tests per provider**, (2) **defined skills**.

### Caching tests (per provider — "each platform")
- **Unit (offline):** assert `CacheStrategy::Auto` request shape — Claude:
  breakpoints after tools, after system+skills, rolling on prev-turn last message,
  ≤ 4; OpenAI: `prompt_cache_key` from `cache_key(ctx)`, stable-first ordering.
  Snapshot the serialized body.
- **Prefix-stability:** two runs with different per-user tails produce a
  byte-identical cached prefix up to the last shared breakpoint.
- **Conversation continuity:** turn N+1 reuses turn N's prefix breakpoint.
- **Live (feature-gated, env keys):** same large stable prefix twice → assert
  Claude `usage.cache_read_tokens > 0` and the OpenAI cached-token field non-zero
  on the second call. Skipped in CI without keys.

### Skills tests
- `list` injects `{name, description}` into the system prompt.
- `search_skills` returns full-text matches; `load_skill(id)` fetches the body on
  demand and nothing before. `FsSkillSource` round-trips fixtures.

### Middleware / context tests
- Each hook fires at the right point; `Flow::Stop`/`Approval::{Deny,Stop}` timing
  + final-text propagation.
- `on_start` loads + prepends history; `on_step_done` checkpoint is consistent
  (no dangling `tool_use`).
- Composition: forward order, first-stop-wins, `on_finish` runs for every started
  middleware (even on stop/error).
- Concurrency: many simultaneous `run()`s share one middleware instance with no
  cross-run state leakage; `set_model` in one run does not affect another.

### Provider / switching tests
- `set_model` resolves model+provider from the registry; switching mid-run
  translates neutral `Message` to each backend (Claude thinking sig dropped for
  OpenAI). Usage accrues per provider into one `Usage`.

### Retry & timeout tests (mock provider)
- Transient (Transport/429/5xx) retried to the cap then `Finish`/error; 4xx not
  retried. Hung provider trips `with_timeout`. `429` honors `Retry-After` (paused
  mock clock). Failure before first token retries; after a delta does not.

Run with `cargo nextest run --workspace`; doctests via `cargo test --workspace
--doc`. Quality gate `./code-check.sh`.

## 20. Open questions

- Exact `Ctx` bounds for built-in conveniences (a `HasSession`/`HasUser` marker)
  vs fully opaque `Ctx`.
- Whether `FsSkillSource` stays in-crate or moves to an example.
- Whether to ship a small `examples/` host runner registry (the §14 pattern) for
  reference, kept clearly outside the crate's public API.
- Graceful cancel: drop-to-cancel only, or also a `CancelToken` carried in `ctx`
  for cooperative tool cancellation.
