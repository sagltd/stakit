# Agent refactor — design

- **Date:** 2026-06-04
- **Crate:** `stakit-ai-sdk` (`crates/ai-sdk`)
- **Status:** approved for planning
- **Type:** green-field refactor — old/non-working abstractions are removed, not preserved.

## 1. Goals

1. `agent.run()` runs the full agentic loop (many steps until the model stops / a
   limit hits / a middleware stops / error). The host never writes a step loop.
2. **The `Agent` is a STATEFUL session object.** It holds the conversation, the
   current model/provider, the registered providers, tools, skills (name +
   description), middleware, cache, and the app context. `run()` mutates it.
3. **Cheap to create** — build an `Agent` per request / cron / sub-agent.
   Provider clients are `Arc` inside, so registering them is a pointer bump.
4. **One** async extension trait — `AgentMiddleware` — for conversation load/save,
   tool approval, stop, model/system switching, observation.
5. **Runtime provider + model switching** — register N providers; change the
   active one mid-run from a middleware via `set_model`.
6. **Automatic, scale-correct prompt caching** for OpenAI and Claude.
7. **Streaming** output (`AgentEvent`) so the host can pipe deltas to the user.
8. Idiomatic Rust: borrowed reads, `Arc<str>`-backed message content, in-place
   mutation, `tracing` spans.

## 2. Non-goals

- **No background runner / run-registry / `Session` / `Conversation` in the SDK.**
  The SDK ends at `agent.run()`. The `Agent` is stateful and cheap; if the host
  wants to run agents in the background, track them, resume, or kill them, that is
  the **host's** concern with its own generic task management. The SDK has none of
  it.
- No provider-side conversation storage (e.g. OpenAI Responses
  `previous_response_id`). We always send from the agent's own conversation.
- Multi-provider failover / load balancing.
- Structured/typed final-output schema (future; §18).
- Tool-output truncation policy (future; §18).

## 3. Mental model — a stateful Agent + a database

There are two things, and **two different "contexts"** — keep them separate.

```
┌──────────────────────────────────────────────────────────────┐
│ AGENT<Ctx> — STATEFUL session object. Cheap to create.        │
│   ctx: Ctx               ← APP context (db, user, session)     │  ← Agent::new(ctx)
│   providers (registry) · current_model · current_provider     │
│   tools · skills (name+description only) · middleware · system │
│   messages: Vec<Message> ← THE CONVERSATION ("llm context")   │  ← mutated by run()
│   cache · retry · usage                                        │
└──────────────────────────────────────────────────────────────┘
        │  agent.run(&mut self)   ← runs the loop, MUTATES self.messages
        ▼
┌──────────────────────────────────────────────────────────────┐
│ DATABASE — where you persist `messages` between agent          │
│   lifetimes, via a middleware. (host-owned)                   │
└──────────────────────────────────────────────────────────────┘
```

**Two contexts (this was the confusing part):**
- **`Ctx`** = your **app context** — db connection, user id, session id, handles.
  Passed once to `Agent::new(ctx)`. Read in hooks/tools via `cx.ctx()`. **Not** the
  LLM conversation.
- **`messages: Vec<Message>`** = the **conversation** (what we kept calling "the
  agent's context"). Lives inside the agent, grows as `run()` executes, reached in
  hooks via `cx.messages()` / `cx.messages_mut()`.

Lifecycle: create the agent (cheap) → seed/load the conversation (`with_context`
or a middleware `on_start`) → `run()` (mutates `messages`, streams events) → a
middleware persists `messages` to db. For a server you typically build a fresh
agent per request and load the conversation from db in a middleware; for a CLI you
keep one agent for the whole session and call `run()` each turn.

**A session is reconstructed, never stored.** There is no persistent session
object in the SDK. To continue a conversation: build a **new** agent, load the
prior conversation from db (a middleware `on_start`), and `run()`. The agent's
statefulness lasts only for its own lifetime; durability lives in your db.

## 4. The `Agent` struct (what it stores)

```rust
pub struct Agent<Ctx> {
    ctx: Ctx,                                          // APP context (db/user/session)
    providers: IndexMap<String, Box<dyn Provider>>,    // registered, keyed by model id
    current_model: String,                             // active model (default or set_model)
    // current provider = providers[current_model]
    tools: Vec<Box<dyn ToolDyn<Ctx>>>,
    skills: Vec<Skill>,                                // {id, name, description} — injected into system
    skill_loader: Option<Box<dyn SkillLoader<Ctx>>>,   // loads body on demand (host impl)
    middleware: Arc<[Box<dyn AgentMiddleware<Ctx>>]>,  // Arc so run() can split borrows
    system: Option<String>,
    messages: Vec<Message>,                            // THE CONVERSATION
    cache: CacheStrategy,                              // prompt-cache config (§13)
    retry: RetryPolicy,                                // §14
    usage: Usage,                                      // accumulated across run()s
}
```

`Agent::new(ctx)` takes the app context. The conversation starts empty (seed it
with `with_context` or load it in a middleware `on_start`).

## 5. What changes vs today

| Concept | Today | Decision |
| --- | --- | --- |
| `Agent<P, Ctx>` immutable + static provider `P` | exists | **stateful `Agent<Ctx>`** (holds conversation/model/...); **drop `P`** → dyn provider registry |
| `agent.run() -> Stream<LoopEvent>` | exists | keep streaming; `run(&mut self)`; rename `LoopEvent` → `AgentEvent`; add `.outcome()` / `IntoFuture` |
| `Tool<Ctx>` + registry + `#[tool]` | exists | keep |
| `SkillLoader { list, load }` | exists | keep the **trait** (host impl); agent caches name+desc, loads body on demand |
| `FsSkillLoader` / `FsSkillSource` | exists | **remove** — loading from a folder is not the SDK's business |
| `ContextLoader` / `FsContextLoader` / `LoadedContext` | exists | **remove** — conversation load/save is a middleware |
| hooks `can_use_tool` / `on_ask` / `prepare_step` | exist | **remove** — folded into `AgentMiddleware` |
| `Permission` enum | exists | → `Approval` (Allow / Deny / Stop) |
| `CacheStrategy` / `CacheTtl` | exists | keep; agent holds it; `Auto` scale-correct (§13) |
| `Usage` / `Pricing` | exists | keep; agent accumulates `usage` |
| `Message` | exists | content `Arc<str>`/`Bytes`; images = `Image { Url | FileId | Base64 }` |

### Removal list
- `context.rs` (`ContextLoader`, `FsContextLoader`, `LoadedContext`).
- `FsSkillLoader` / any folder-based skill source.
- The three builder hook closures + their type aliases.
- `Permission` (→ `Approval`).
- The static provider generic `P` on `Agent`.
- Rename `LoopEvent`/`FinishReason` → `AgentEvent`/`Finish`.

## 6. Public surface

```rust
// build a stateful agent — cheap; do it per request / cron / sub-agent
let mut agent = Agent::new(ReqCtx { db, user, session })   // ← APP ctx (db/user/session)
    .provider(OpenAiProvider::new(client.clone()))          // default provider (registered)
    .register_provider(ClaudeProvider::new(client.clone())) // more providers
    .register_provider(LocalProvider::new(client.clone()))
    .model("gpt-5")                                         // default model
    .system("You are helpful.")
    .register_tools([Search, Write])                        // refs / cheap
    .register_middleware(MyDbContext)                       // loads/saves the conversation
    .skills(MyDbSkills)                                     // host SkillLoader (name+desc + load body)
    .cache(CacheStrategy::Auto)                             // §13
    .with_retries(5)
    .with_timeout(Duration::from_secs(30))
    .with_context(vec![Message::user("hi")]);               // seed the new turn (or load in on_start)

// run — the full agentic loop; MUTATES agent.messages; many steps until stop/limit/error
let out = agent.run().await?;                               // IntoFuture → Outcome
println!("{}", out.text);

// or stream deltas to the user:
let mut run = agent.run();
while let Some(ev) = run.next().await { /* pipe AgentEvent over SSE/WS */ }
```

Builder methods consume and return `Self` (no separate `build()`); the resulting
value **is** the agent. `run` takes `&mut self`.

```rust
impl<Ctx: Send + Sync + 'static> Agent<Ctx> {
    pub fn new(ctx: Ctx) -> Self;                          // app context

    pub fn provider(self, p: impl Provider) -> Self;       // register + make default
    pub fn register_provider(self, p: impl Provider) -> Self;
    pub fn model(self, id: impl Into<String>) -> Self;     // default model
    pub fn system(self, text: impl Into<String>) -> Self;
    pub fn register_tool(self, t: impl Tool<Ctx>) -> Self;
    pub fn register_tools<I: IntoIterator<Item = impl Tool<Ctx>>>(self, tools: I) -> Self;
    pub fn register_middleware(self, m: impl AgentMiddleware<Ctx>) -> Self;
    pub fn skills(self, loader: impl SkillLoader<Ctx>) -> Self;
    pub fn with_context(self, messages: Vec<Message>) -> Self;   // seed the conversation
    pub fn cache(self, c: CacheStrategy) -> Self;
    pub fn cache_key(self, f: impl Fn(&Ctx) -> Option<Arc<str>> + Send + Sync + 'static) -> Self;
    pub fn with_retries(self, n: u32) -> Self;
    pub fn with_timeout(self, d: Duration) -> Self;

    pub fn push(&mut self, m: Message);                    // add a turn before run()
    pub fn run(&mut self) -> AgentRun<'_>;                 // mutates self.messages
    pub fn messages(&self) -> &[Message];                  // read the conversation
    pub fn usage(&self) -> &Usage;
}
```

- `Agent::new` is **cheap**: it stores handles. Provider clients must be `Arc`
  inside (e.g. a shared `reqwest::Client`) so per-request agents reuse the
  connection pool — the host holds the clients and clones them in.
- `run(&mut self)` borrows the agent exclusively for the run's duration and
  updates `messages`/`usage` in place. A second turn = call `run()` again.

### `AgentRun`, events, outcome, cancellation

```rust
pub struct AgentRun<'a> { /* borrows &'a mut Agent; impl Stream<Item = AgentEvent> + IntoFuture */ }
impl AgentRun<'_> { pub async fn outcome(self) -> Result<Outcome, AgentError>; }

pub enum AgentEvent {
    StepStart  { index: u32 },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCall   { id: String, name: String, args: Value },
    ToolResult { id: String, name: String, result: ToolOutcome },
    StepEnd    { index: u32, text: String, reasoning: Option<String>, usage: Usage, cost: Option<f64> },
    Done(Outcome),
}

pub struct Outcome {
    pub text: String,            // final text — OR a middleware Stop message
    pub usage: Usage,            // accumulated
    pub cost: Option<f64>,       // accumulated USD
    pub steps: u32,
    pub finish: Finish,
}
pub enum Finish { EndTurn, Limit(StopCond), Stopped { message: Option<String> }, Cancelled }
```

- The full conversation after a run is `agent.messages()` (no need to return it).
- **Cancellation = drop the `AgentRun`** (and/or the task holding the agent). The
  run stops at the next await; the in-flight provider request aborts. The SDK
  needs no handle registry — dropping is the cancel.

## 7. `AgentMiddleware` — the one extension trait

Stored as `Arc<[Box<dyn AgentMiddleware<Ctx>>]>` → dyn-safe → `#[async_trait]`
(hooks fire ~once per step; the box is negligible). The `Arc` lets `run()` iterate
the middleware while mutably borrowing the rest of the agent (split borrow).

```rust
#[async_trait]
pub trait AgentMiddleware<Ctx>: Send + Sync + 'static {
    /// Before the first model call. Load the conversation / inject guidance.
    async fn on_start(&self, cx: &mut AgentCx<Ctx>) -> Result<Flow, AgentError> { Ok(Flow::Continue) }
    /// Before each model call. Switch model/system, check budget, drain queued input, compact.
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

### `AgentCx<Ctx>` — what hooks get (the two contexts, both reachable)

```rust
impl AgentCx<Ctx> {
    fn ctx(&self) -> &Ctx;                  // APP context (db/user/session) — read-only
    fn messages(&self) -> &[Message];       // THE CONVERSATION — borrow (zero copy)
    fn messages_mut(&mut self) -> &mut Vec<Message>;  // load / inject / prepend / compact
    fn usage(&self) -> &Usage;              // accumulated
    fn cost(&self) -> Option<f64>;          // accumulated USD
    fn index(&self) -> u32;                 // current step
    fn step(&self) -> Option<&Step>;        // last completed step (in on_step_done)
    fn set_model(&mut self, id: impl Into<String>);    // switch model+provider for this run
    fn set_system(&mut self, text: impl Into<String>); // switch system for this run
}

pub struct Step { pub index: u32, pub reasoning: Option<String>, pub text: String,
                  pub tool_calls: Vec<ToolCallRecord>, pub stop: StopReason }
pub struct ToolCallRecord { pub id: String, pub name: String, pub args: Value,
                            pub approval: Approval, pub result: ToolOutcome, pub elapsed: Duration }
pub enum ToolOutcome { Ok(Value), Denied { message: String }, Error(String) }
```

- A middleware `&self` may be shared across agents — keep only shared handles on
  it; per-run/per-conversation data lives in the agent (`cx.ctx()`,
  `cx.messages()`).
- `set_model` / `set_system` mutate the agent's `current_model` / `system` for the
  rest of the run; `set_model` resolves a different registered provider (§12). A
  model switch resets that provider's cache (expected).

### Composition (locked)
- `on_start` / `on_step` / `on_step_done`: registration order; first `Flow::Stop`
  halts and skips the rest of that hook.
- `on_tool_approve`: registration order; first non-`Allow` wins (Stop > Deny > Allow).
- `on_finish`: registration order; runs for every middleware whose `on_start`
  ran, even on stop/error.
- Stop takes effect at the next hook boundary (no mid-stream kill; drop = hard
  cancel).

## 8. The conversation: load/save via middleware (no Context trait)

The conversation is `agent.messages` (`Vec<Message>`), reached via
`cx.messages()` / `cx.messages_mut()`. Loading from db and saving back are a
**middleware**, not a trait.

**The middleware owns all conversation policy — the SDK dictates none.** The SDK
provides only the hooks and `messages_mut()`. *When* to load, whether to inject
memory/guidance, whether and how to compact / summarize / trim / drop old turns,
under *what* conditions (token count, step count, cost, time) — all of it is the
host's middleware code. The SDK never decides any of this; it just hands you
mutable access to the conversation at each hook.

```rust
struct MyDbContext;
#[async_trait]
impl AgentMiddleware<ReqCtx> for MyDbContext {
    async fn on_start(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        let prior = cx.ctx().db.load(&cx.ctx().session).await.map_err(AgentError::context)?;
        cx.messages_mut().splice(0..0, prior);     // prepend prior before the new turn
        Ok(Flow::Continue)
    }
    async fn on_step(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        cx.set_model(cx.ctx().chosen_model.clone());          // per-user model+provider
        if cx.cost().unwrap_or(0.0) >= cx.ctx().budget {      // stop on budget
            return Ok(Flow::Stop("You've used all tokens for your subscription.".into()));
        }
        Ok(Flow::Continue)
    }
    async fn on_step_done(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        cx.ctx().db.save(&cx.ctx().session, cx.messages()).await.map_err(AgentError::context)?;
        Ok(Flow::Continue)                                    // checkpoint — consistent at step boundary
    }
}
```

**Checkpoint only at `on_step_done`** — at a step boundary every `tool_use` has
its `tool_result`, so the persisted conversation is always consistent. Crash
mid-step ⇒ next agent for that session reloads the last clean checkpoint, losing
only the partial step. Queued mid-run input: a middleware drains a channel from
`cx.ctx()` in `on_step` and `cx.messages_mut().push(...)`.

## 9. Tools

`Tool<Ctx>` (typed, `#[tool]` macro) unchanged; stored erased as
`Box<dyn ToolDyn<Ctx>>`. Tool bodies get `&AgentCx<Ctx>` and read the app context
via `cx.ctx()`. Deferred tools + built-in `tool_search` retained.

## 10. Skills — `SkillLoader` (host impl), agent holds name+description

The agent stores **only `{id, name, description}`** for each skill and injects
those into the system prompt (progressive disclosure). The body is fetched on
demand by a built-in tool. There is **no folder-based source** — the host
provides a `SkillLoader` (db, fs, anywhere).

```rust
pub struct Skill { pub id: String, pub name: String, pub description: String }  // NO body

#[async_trait]
pub trait SkillLoader<Ctx>: Send + Sync {
    async fn list(&self, ctx: &Ctx) -> Result<Vec<Skill>, AgentError>;        // name+desc only
    async fn load(&self, ctx: &Ctx, id: &str) -> Result<SkillContent, AgentError>;  // body on demand
}
pub struct SkillContent { pub body: String, pub references: Vec<String> }
```

On the first run the agent calls `list()` (using `agent.ctx`), caches the result
in `agent.skills`, and injects the name+description list into the system prompt.
Built-in tools `search_skills(query)` (full-text over the cached list) and
`load_skill(id)` (calls `loader.load`) are auto-registered when a loader is set.
**`FsSkillLoader`/`FsSkillSource` are removed.**

## 11. Providers — dyn registry, runtime switch, neutral message

The agent holds `IndexMap<String, Box<dyn Provider>>` keyed by model id (`P`
generic dropped). `Provider` is erased (erase associated `Raw`, return
`BoxFuture`). Dyn dispatch is free next to the network HTTP boundary.

- `.provider(p)` registers + sets default; `.register_provider(p)` registers more
  (keyed by `p.model_id()`). `.model(id)` sets the default model.
- `cx.set_model(id)` switches the active model **and** provider for the rest of
  the run.
- **Cross-provider switch mid-conversation** (Claude↔OpenAI): the conversation is
  stored as **neutral `Message`**; each provider adapter translates at send time,
  down-converting what its backend can't represent (e.g. drop Claude thinking
  signatures when sending to OpenAI). Switching resets that provider's cache; new
  tool-call ids issue going forward; usage accrues per provider into `agent.usage`.
- **Single API key, 10k users** → the bottleneck is provider **rate limits**, not
  the agent — bound concurrency host-side (semaphore) + retry 429 (§14).

### Message + images

```rust
pub struct Message { /* role + content; text as Arc<str> for cheap clone */ }
pub enum Image {
    Url(Arc<str>),                                // DEFAULT — cheap, RAM-friendly, re-send-cheap
    FileId(Arc<str>),                             // uploaded once (provider Files API) — best for repeats
    Base64 { media_type: Arc<str>, data: Bytes }, // only when no URL; big; Bytes = cheap clone
}
```

Prefer `Url`/`FileId`: base64 bloats every request, every cache-write, and RAM.
The conversation stays small (URLs/ids, not megabytes). `Arc<str>`/`Bytes` content
makes cloning a `Message` a pointer bump.

## 12. Caching

The agent holds `cache: CacheStrategy`. Two facts: caching saves **cost only**,
and we **always resend the conversation** (exact-prefix match; no provider-side
state).

Provider facts (verified):
- **Claude:** `cache_control:{type:"ephemeral"}` breakpoints, max 4, prefix-based,
  exact content match. Min 1024/2048 tok. TTL 5 min / 1 h beta. Write 1.25×/2×
  input; read **0.1×**. Order: tools → system → messages.
- **OpenAI:** automatic, ≥1024 tok, prefix match, routed by org + optional
  `prompt_cache_key`. ~50–75% off cached input.

`CacheStrategy::Auto`:
- order content **stable → variable**: `tools → system → skills-manifest →
  shared-instructions → [per-user memory] → conversation`.
- **Claude:** breakpoint after tools, after system+skills (global shared prefix),
  rolling breakpoint on the previous turn's last message. ≤ 4.
- **OpenAI:** set `prompt_cache_key` from `cache_key(&agent.ctx)`; rely on auto
  prefix caching with the ordering above.

**Stability rule:** anything before a breakpoint must be byte-stable across turns
or it busts the cache (shared static → cached across users; stable-per-convo →
between global and rolling breakpoints; volatile → after all breakpoints). Cache
expiry is harmless (we always resend → provider recomputes, full input that turn).
`cache_key` = a closure over the app `Ctx` (typically the session id).

## 13. Retry & timeout

`agent.retry: RetryPolicy` — wraps each provider call (not a middleware).

```rust
.with_retries(5)                       // attempts on transient errors (default 2)
.with_timeout(Duration::from_secs(30)) // PER-ATTEMPT; a hung connection trips it
```

Retryable: `Transport`, per-attempt timeout, `Api{429}` (honor `Retry-After`),
`Api{5xx}`. Not: `Api{4xx≠429}`, `InvalidArgument`, `Decode`, `ToolError`,
`Cancelled`. Default backoff = exponential + full jitter. **Streaming caveat:**
retry only before the first token; a mid-stream failure surfaces as error (no
duplicated output).

## 14. Background, resume, cron — host's job (SDK has nothing)

The `Agent` is stateful and cheap. The SDK provides only `agent.run()`. To run in
the background, the host moves an agent into a spawned task and tracks that task
however it likes; to stop, it drops/aborts the task; to resume after a crash or on
a cron tick, it builds a fresh agent and a middleware reloads the conversation
from db (`run()` continues). **The SDK has no runner, registry, handle, or session
type.** This keeps the SDK surface to: build agent → `run()`.

## 15. Concurrency & memory model (10k users / one process)

- Build an agent **per request** (cheap). Provider clients are `Arc` (the host
  holds them, clones into each agent → shared connection pool).
- The conversation is in RAM only while that agent's `run()` executes; a
  middleware loads it from db at start and saves each step; the agent is dropped
  after. **Idle sessions = 0 RAM** (conversation in db).
- With one API key, concurrent runs are **rate-limit-bound** (a few hundred, not
  10k) → tens of MB. The db holds the idle 9,000+.
- Bound concurrency host-side with a `tokio::Semaphore` around `run()`.
- Zero-copy: `cx.messages()` borrows `&[Message]`; the agent appends owned new
  messages in place; `Message` content is `Arc<str>`/`Bytes`. The only unavoidable
  allocation is the JSON request body at send.

## 16. Tracing

1. **`tracing` spans:** span per run (tagged via `cache_key`/session), child span
   per step / tool / provider call; record accumulated usage+cost on close.
2. **`AgentEvent` stream:** live trace to the user.
3. **`on_step_done` middleware:** durable custom trace (write `Step` to db).

## 17. Dependencies

- Add `async-trait` to `[workspace.dependencies]` (pin newest — check crates.io).
- `tracing` already in the workspace (`0.1.44`); add to the crate for spans.
- `indexmap` (present) for the ordered provider registry.

## 18. Deferred (future passes)

- Structured/typed final-output schema.
- Tool-output truncation policy.

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
  Claude `usage.cache_read_tokens > 0` and OpenAI cached-token field non-zero on
  the second call. Skipped in CI without keys.

### Skills tests
- `list` populates `agent.skills` and injects name+description into the system
  prompt. `search_skills` returns full-text matches; `load_skill(id)` calls
  `loader.load` on demand and nothing before.

### Middleware / conversation tests
- Each hook fires at the right point; `Flow::Stop`/`Approval::{Deny,Stop}` timing
  + final-text propagation.
- `on_start` loads + prepends the conversation; `on_step_done` checkpoint is
  consistent (no dangling `tool_use`).
- Composition: forward order, first-stop-wins, `on_finish` runs for every started
  middleware (even on stop/error).
- `set_model` in one agent's run does not affect another agent.

### Provider / switching tests
- `set_model` switches active model+provider from the registry; switching mid-run
  translates neutral `Message` per backend (Claude thinking sig dropped for
  OpenAI). Usage accrues into `agent.usage`.

### Retry & timeout tests (mock provider)
- Transient (Transport/429/5xx) retried to the cap then error; 4xx not retried.
  Hung provider trips `with_timeout`. `429` honors `Retry-After` (paused mock
  clock). Failure before first token retries; after a delta does not.

Run with `cargo nextest run --workspace`; doctests via `cargo test --workspace
--doc`. Quality gate `./code-check.sh`.

## 20. Open questions

- Exact `Ctx` bounds for built-in conveniences (a `HasSession`/`HasUser` marker)
  vs fully opaque `Ctx`.
- `run()` ergonomics: `run()` over a pre-seeded conversation only, or also a sugar
  `run_with(msg)` that pushes then runs.
- Graceful cancel: drop-to-cancel only, or also a `CancelToken` carried in `ctx`
  for cooperative tool cancellation.
