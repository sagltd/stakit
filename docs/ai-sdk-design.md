# stakit-ai-sdk — Design Spec

Status: **design / API sketch** (no implementation yet). This document captures the
agreed direction from the design QA. Code is written only after this is approved.

`stakit-ai-sdk` is a set of **provider-agnostic primitives for building agents** — not a
rigid agent. Strong agents (goal-completion loops, coding agents, etc.) are built on top
of these primitives by the consumer.

---

## 1. Goals & requirements

| # | Requirement | How it's met |
|---|---|---|
| 1 | Primitives, not a rigid agent | Composable `Provider` / `Tool` / `Agent` building blocks |
| 2 | Provider trait — public, minimal, trivially 3rd-party-impl | `provider.rs` trait + reference impls behind features |
| 3 | `#[tool]` macro — `(cx, args)` / `(args)` / `()`, sync or async | Clones `#[action]` macro mechanics |
| 4 | Tool params schema from a type, never hand-written | `#[derive(Model, JsonSchema)]` → JSON Schema via model-derive |
| 5 | Per-parameter descriptions for tools | `///` doc-comments on fields + `#[arg(description=)]` override |
| 6 | Tools get context; can call anything | Generic `ToolCx<Ctx>` — `Ctx` is the consumer's world |
| 7 | Dynamic tools — add to a live agent | Mutable registry + `ToolSet` bundles |
| 8 | Tool permissions — async, routable to a human | `can_use_tool` → `Allow/Deny/Ask`; `on_ask` async resolver |
| 9 | MCP via JSON config (stdio + http), dynamically refreshable | `McpMode::Client` (default) wraps MCP into our tool system |
| 10 | Skills — pluggable source, lazy bodies | `SkillSource` trait; `FsSkillSource` = `.agents/skills` default |
| 11 | Agent loop = injectable async event stream | `Stream<LoopEvent>` + `prepare_step` + mpsc inject channel |
| 12 | Per-iteration telemetry — tokens + cost | `LoopEvent::Usage { usage, cost }` via `Pricing` table |
| 13 | Cancellation — abort in-flight LLM call | `CancelToken` into provider call; drops the stream |
| 14 | Context + caching — maximize token savings | Stable append-only prefix + `CacheStrategy` + compaction |
| 15 | Scale to 100k tools | Deferred registry + built-in `tool_search` (mirrors Anthropic) |

**No coupling to `stakit-router`.** The core depends on nothing from router. It works with
any consumer (router, axum, plain CLI, anything) via the generic `Ctx`.

---

## 2. Locked design decisions

1. **Coupling — router-agnostic, generic `Ctx`.** No `router` feature, no glue crate.
   Client-tools and human-approval are wired by the consumer through `Ctx` + the `on_ask`
   resolver. A router example lives in `examples/`, never in the lib.
2. **Schema — extend `model-derive`, no `schemars`.** New `JsonSchema` trait in `model` +
   `emit_jsonschema.rs` walking the existing `Ir`. Opt-in derive; only tool-arg structs pay.
3. **Param docs — `///` doc-comments → schema `description`**, with `#[arg(description=)]`
   override. Requires a small `description` slot added to the model-derive `Field` IR.
4. **Transport — agnostic loop.** Core loop runs anywhere. Client-tools / human-approval
   light up purely from what the consumer puts in `Ctx`; no transport assumption in the lib.
5. **MCP — client mode default.** We host the MCP client (stdio + http/sse), `list_tools`,
   wrap each as our `ToolDyn`. `McpMode::Native` (remote-URL passthrough, provider executes)
   is an opt-in bonus. This matches OpenAI Agents SDK and Claude Agent SDK exactly.
6. **Tools — flat registry + `ToolSet` bundles + `tags` + deferred entries + nestable.**
   No mandatory group taxonomy. A "router tool" / sub-agent-as-tool is just a `ToolDyn` that
   owns a sub-registry — recursive, no new concept.

---

## 3. Crate layout

```
crates/
  ai-sdk/                      # stakit-ai-sdk — runtime core (router-agnostic)
    src/
      lib.rs
      message.rs               # unified Message / content blocks
      provider.rs              # Provider trait + ChatRequest/ChatResponse/StreamEvent
      provider/
        claude.rs              # feature = "claude"
        openai.rs              # feature = "openai" (Chat + Responses impls)
      tool.rs                  # Tool trait, ToolDyn, ToolSet, ToolRegistry, tool_search
      cx.rs                    # ToolCx<Ctx>, Permission
      agent.rs                 # Agent<P, Ctx> + builder + run() event stream
      loop_event.rs            # LoopEvent, StopCond, FinishReason
      usage.rs                 # Usage + Pricing + cost
      cache.rs                 # CacheStrategy
      skill.rs                 # SkillSource trait + FsSkillSource
      mcp.rs                   # McpConfig + McpClient + McpToolDyn + McpMode
      error.rs                 # AiError, ProviderError, ToolError
  ai-sdk-derive/               # stakit-ai-sdk-derive — #[tool] attribute macro

# changes to existing crates:
crates/model/src/json_schema.rs          # NEW: JsonSchema trait + scalar/collection impls
crates/model-derive/src/emit_jsonschema.rs # NEW: third emitter, walks the same Ir
crates/model-derive/src/ir.rs              # CHANGE: Field gains `description: Option<String>`
```

Workspace deps to reuse: `serde`, `serde_json`, `futures`, `async-stream`, `tokio`,
`thiserror`, `hashbrown`, `indexmap`, `regex`; `syn`/`quote`/`proc-macro2` for the macro
crate. New deps (pin latest from crates.io): an HTTP client (`reqwest`), an MCP client
crate, and a YAML parser for skills (avoid unmaintained `serde_yaml` — use a maintained one).

---

## 4. Unified message model (`message.rs`)

System prompt is a request field, not a message (matches both vendors). Tool-call input is
kept as a parsed `serde_json::Value` (Anthropic-native; OpenAI parsed on ingest, stringified
on egress). Responses always keep a `raw` escape hatch.

```rust
pub enum Message { User(Vec<UserContent>), Assistant(Vec<AssistantContent>) }

pub enum UserContent {
    Text(String),
    Image(ImageSource),
    ToolResult { id: String, content: Vec<ToolResultPart>, is_error: bool },
}
pub enum AssistantContent {
    Text(String),
    ToolUse { id: String, name: String, input: serde_json::Value },   // already parsed
    Thinking(Thinking),                                               // signature round-trips
}
pub enum Thinking { Visible { text: String, signature: Option<String> }, Redacted { data: String } }
pub enum ToolResultPart { Text(String), Image(ImageSource) }
pub enum ImageSource { Base64 { media_type: String, data: String }, Url(String) }
```

**The serializer (per provider) owns tool-result ordering and batching** — the caller never
arranges blocks. Anthropic needs all parallel results in one user turn, blocks-first;
OpenAI Chat needs `role:tool` messages; OpenAI Responses needs `function_call_output` items.

---

## 5. Provider trait (`provider.rs`)

Native `async fn` in trait (edition 2024). Public + minimal so a 3rd party impls a new
provider (Gemini/Mistral/Ollama/local) in a short file. `Raw` escape hatch always kept.

```rust
pub trait Provider: Clone + Send + Sync + 'static {
    type Raw: Send + Sync;
    fn complete(&self, req: ChatRequest)
        -> impl Future<Output = Result<ChatResponse<Self::Raw>, ProviderError>> + Send;
    fn stream(&self, req: ChatRequest)
        -> impl Future<Output = Result<
            BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>> + Send;
}

pub struct ChatRequest {
    pub model: String,
    pub system: Option<SystemPrompt>,        // string + optional cache breakpoint
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,                 // ACTIVE tools for this step (see deferred §7)
    pub tool_choice: ToolChoice,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub stop_sequences: Vec<String>,
    pub thinking: ThinkingConfig,            // Off | Adaptive | Budget(u32)
    pub cache: CacheStrategy,                // §9
    pub extra: serde_json::Map<String, serde_json::Value>,  // vendor passthrough
}
pub struct ChatResponse<R> {
    pub content: Vec<AssistantContent>,
    pub stop: StopReason,
    pub usage: Usage,
    pub raw: R,
}
pub enum StopReason { EndTurn, MaxTokens, StopSequence(String), ToolUse, Refusal, Pause, Other(String) }

pub enum StreamEvent {
    Start { usage: Usage },                  // partial input usage (Anthropic message_start)
    TextDelta(String),
    ReasoningDelta(String),
    SignatureDelta(String),
    ToolCall { id: String, name: String, input: serde_json::Value },  // accumulated WHOLE
    End { stop: StopReason, usage: Usage },
}
```

Each provider adapter **accumulates partial-JSON tool-arg fragments internally** and only
emits a `ToolCall` once complete. That normalization is the single biggest value of the SDK.
Usage caveat baked into adapters: Anthropic `output_tokens` in `message_delta` is cumulative
(replace, not add); OpenAI accumulates per chunk with trailing usage.

**OpenAI = two impls:** `OpenAiResponsesModel` (default agentic target, closest to Anthropic's
block/item model, native MCP) + `OpenAiChatModel` (compatibility for OpenAI-compatible
endpoints). `ClaudeModel` for Anthropic. `*Client` mints cheap per-model handles (rig pattern).

---

## 6. Tools (`tool.rs` + `#[tool]` macro)

Typed `Tool` + object-safe `ToolDyn<Ctx>` twin + blanket impl (mirrors `Action`/`ErasedAction`).
Boxed futures (codebase idiom), not async-fn-in-trait, so the trait stays object-safe.

```rust
pub trait Tool<Ctx>: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Validate + Send;
    type Output: Serialize + Send;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn run<'a>(&'a self, cx: &'a ToolCx<Ctx>, args: Self::Args)
        -> BoxFuture<'a, Result<Self::Output, ToolError>>;
}

pub(crate) trait ToolDyn<Ctx>: Send + Sync {
    fn def(&self) -> ToolDef;                                   // name + desc + Args::schema()
    fn call_json<'a>(&'a self, cx: &'a ToolCx<Ctx>, args: Value)
        -> BoxFuture<'a, Result<Value, ToolError>>;
}

pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,       // JSON Schema, built once at registration
    pub strict: bool,
    pub cache: bool,                         // attach cache_control (Anthropic)
    pub tags: Vec<String>,                   // for search/scoping
    pub defer: bool,                         // deferred (name+desc only until searched) §7
}
pub enum ToolChoice { Auto, Any, Required, None, Tool(String), DisableParallel(Box<ToolChoice>) }
```

**`#[tool]` accepts the same four shapes as `#[action]`** (reuses `cx_info`/`result_parts`/
`stream_item_types` + the cx-present-vs-absent generic header trick):

```rust
#[tool]                        async fn weather(cx: &ToolCx<App>, args: WeatherArgs) -> Result<Report, ToolError>
#[tool]                        async fn now(args: TzArgs) -> Result<String, ToolError>
#[tool(description = "ping")]  fn ping() -> Result<&'static str, ToolError>      // sync, no cx, no args
```

Deltas vs `#[action]`: parse real attr args via `syn` (`name`, `description`, `tags`);
`description` defaults to the fn `///` doc; emit the `Tool`/`ToolDyn` path; register
`Args::schema()` as `parameters`.

**Args type** uses `#[derive(Model, JsonSchema)]` — `validate()` (post-deserialize tool-arg
check, `Error::validation` on failure → model retries), `to_ts()`, and `schema()` all derive
from one IR. Field `///` docs → JSON Schema property `description`.

**Round-trip:** model emits `ToolUse{id,name,input}` → registry lookup by `name` → `can_use_tool`
gate → `from_value::<Args>` (parse failure ⇒ `is_error` result, model retries) → `run` →
serialize `Output` → `ToolResult{id, content, is_error}`. All parallel results into ONE user turn.

---

## 7. Tool registry, bundles, scaling to 100k (`tool.rs`)

Flat registry. Bundles and tags on top. Deferred entries + built-in search for scale.

```rust
pub trait ToolSet<Ctx> { fn tools(self) -> Vec<Box<dyn ToolDyn<Ctx>>>; }  // bundle

// registration
Agent::builder(provider)
    .register(PingTool)                 // single tool
    .register_set(WebTools)             // a bundle ("web group")
    .register_set(CodingTools.tagged("coding"))
    .register_set(mcp_client)           // an MCP server is just a ToolSet
```

**Deferred tools + `tool_search` (mirrors Anthropic Tool Search Tool, provider-agnostic):**
- Deferred entries sit in the registry as **name + short desc + tags only** — their full JSON
  Schema is NOT sent in the prompt.
- The SDK ships a built-in `ToolSearchTool`. The model calls it; we regex/substring-search
  over name·desc·tags; matched tools **activate** — their full schemas enter the next step's
  `ChatRequest.tools`.
- Works with **any provider** (our own tool). ~85%-class token reduction at 100k-tool scale.
- Bonus: when provider = Anthropic + `McpMode::Native`, also map to server-side `defer_loading`
  + `tool_search_tool_regex_*`.

**Router-tool / sub-agent-as-tool:** a `ToolDyn` that owns its own sub-registry and searches +
dispatches into it. Recursive composition, no new concept.

---

## 8. Context + permissions (`cx.rs`)

`Ctx` is the consumer's type — the SDK never constrains it. `ToolCx` wraps it + agent-scoped
handles (cancellation, progress emit).

```rust
pub struct ToolCx<Ctx> {
    ctx: Ctx,
    cancel: CancelToken,
    // progress/emit handle ...
}
impl<Ctx> ToolCx<Ctx> {
    pub fn ctx(&self) -> &Ctx;
    pub fn cancelled(&self) -> bool;
}

pub enum Permission { Allow, Deny { reason: String }, Ask }
```

**Client-tools and human-approval are pure consumer wiring — trivial, no SDK feature:**

```rust
struct MyCtx { client: MyWebsocketHandle, db: Db }

#[tool]                                              // a "client tool"
async fn pick_file(cx: &ToolCx<MyCtx>, args: PickArgs) -> Result<String, ToolError> {
    cx.ctx().client.call("pick_file", args).await   // consumer's transport
}

Agent::builder(provider)
    .can_use_tool(|name, input, cx: &ToolCx<MyCtx>| async move {
        if is_dangerous(name) { Permission::Ask } else { Permission::Allow }
    })
    .on_ask(|call, cx: &ToolCx<MyCtx>| async move { // resolves Ask -> consumer decides
        if cx.ctx().client.confirm(&call.name, &call.input).await {
            Permission::Allow
        } else {
            Permission::Deny { reason: "user rejected".into() }
        }
    });
```

`Deny` (or `Ask` resolving to `Deny`) ⇒ loop synthesizes an `is_error` tool_result with the
reason; the tool never executes.

---

## 9. Agent + loop (`agent.rs`, `loop_event.rs`)

Builder + name-keyed registry (mirrors `Router::builder`). Loop = `async-stream` event stream.

```rust
pub struct Agent<P: Provider, Ctx> { /* provider, registry, pricing, stop_when,
                                        can_use_tool, on_ask, prepare_step, skills, cache */ }

impl<P: Provider, Ctx: Send + Sync + 'static> Agent<P, Ctx> {
    pub fn builder(provider: P) -> AgentBuilder<P, Ctx>;
    pub fn run(&self, history: Vec<Message>, cx: Ctx, cancel: CancelToken)
        -> impl Stream<Item = LoopEvent> + Send;
}

pub enum LoopEvent {
    StepStart { step: u32 },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, output: Value, is_error: bool },
    Usage { step: u32, usage: Usage, cost: Option<f64> },    // per-iteration telemetry
    StepEnd { step: u32, stop: StopReason },
    Compacted { freed_tokens: u64 },
    Done { final_text: String, total: Usage, total_cost: Option<f64>, reason: FinishReason },
}
pub enum StopCond { StepCountIs(u32), HasToolCall(String), BudgetUsd(f64),
                    Custom(Arc<dyn Fn(&LoopState) -> bool + Send + Sync>) }
pub enum FinishReason { EndTurn, StopCondition, MaxSteps, MaxBudget, Cancelled, Error }
```

**Per-step cycle:** `prepare_step` hook (rewrite/compact messages, switch model, restrict
active tools, force tool_choice) → `StepStart` → `provider.stream(req)` forwarding deltas →
accumulate whole `ToolCall`s → `can_use_tool` gate each → execute allowed (read-only
concurrently, mutating sequentially) → all results into one `User` turn → `Usage` + `StepEnd`.
Repeat until natural `EndTurn` or any `stop_when` matches. Default guard `StepCountIs(20)`.

- **Mid-loop injection (req 11):** `run` pairs with an `mpsc::Receiver<Message>`; `prepare_step`
  drains pending input before the next provider call.
- **Cancellation (req 13):** `CancelToken` → cancel drops the SSE stream → `Done{Cancelled}`,
  no half-applied tool.

---

## 10. Usage + cost (`usage.rs`)

```rust
#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,           // uncached input
    pub output_tokens: u64,
    pub cache_create_tokens: u64,    // Anthropic only
    pub cache_read_tokens: u64,      // Anthropic cache_read / OpenAI cached_tokens
    pub reasoning_tokens: u64,
}
pub struct ModelPrice { pub input: f64, pub output: f64, pub cache_read: f64, pub cache_write: f64 } // $/1e6
pub struct Pricing(HashMap<String, ModelPrice>);   // bundled table + override
impl Pricing { pub fn cost(&self, model: &str, u: &Usage) -> Option<f64>; }  // documented ESTIMATE
```

Surfaced per iteration via `LoopEvent::Usage`, plus a running cumulative in `Done`. Dedup
gotcha: Anthropic parallel tool calls may emit multiple messages sharing one id with identical
usage — adapter dedups by id before merging.

---

## 11. Skills (`skill.rs`) — pluggable loader + progressive disclosure

Skills follow **3-level progressive disclosure** (verified against Anthropic Agent Skills):

1. **Discovery** — only `name` + `description` of skills enter the agent context. Bodies are
   NOT loaded. At thousands of skills, even manifests are not dumped — a `search_skills` tool
   surfaces matches on demand (same model as deferred tools / `tool_search`).
2. **Activation** — the model calls the built-in **`load_skill(name)`** tool → loader fetches
   that one skill's full body → returned as a tool_result, entering context.
3. **Execution** — referenced files load via built-in **`read_skill_resource(skill, path)`**,
   only when the body points at them.

**The loader is a consumer-provided layer**, invoked **at the start of every `run()`** (before
the first iteration) — so skills are dynamic per run and can come from files, a server, a DB,
anywhere. It separates cheap manifest *listing* from on-demand body *fetching* so bodies never
sit in memory/context until used.

```rust
pub trait SkillLoader<Ctx>: Send + Sync {
    /// Level 1 — cheap. Manifests (name + description) only, NEVER bodies.
    fn list(&self, cx: &ToolCx<Ctx>)
        -> BoxFuture<'_, Result<Vec<SkillManifest>, SkillError>>;
    /// Level 2 — fetch ONE skill body on demand (backs the built-in `load_skill` tool).
    fn load(&self, name: &str, cx: &ToolCx<Ctx>)
        -> BoxFuture<'_, Result<SkillContent, SkillError>>;
    /// Level 3 — fetch a referenced resource (backs `read_skill_resource`). Default: unsupported.
    fn resource(&self, skill: &str, path: &str, cx: &ToolCx<Ctx>)
        -> BoxFuture<'_, Result<String, SkillError>> { /* default Err(Unsupported) */ }
}

pub struct SkillManifest { pub name: String, pub description: String, pub allowed_tools: Vec<String> }
pub struct SkillContent  { pub body: String, pub references: Vec<String> }  // references = relative paths

pub struct FsSkillLoader { root: PathBuf }   // default helper: <root>/.agents/skills/*/SKILL.md
```

**Built-in skill tools** (auto-registered when a loader is set, like `tool_search`):
- `load_skill(name)` → `loader.load(name)` → body into context.
- `read_skill_resource(skill, path)` → `loader.resource(...)`.
- `search_skills(query)` → returns matching manifests; auto-enabled when manifest count exceeds
  a threshold so the system prompt isn't bloated by thousands of names.

`FsSkillLoader`: `list()` globs `*/SKILL.md` and parses only the YAML frontmatter head
(`name`, `description`, optional `license`, `compatibility`, `metadata.{author,version:String}`,
`allowed-tools` space-separated) — never reads bodies; `load()` reads the one `SKILL.md` body;
`resource()` reads `references/<path>`; optional sha256 verify against `skills-lock.json`.
Pluggable — embedded consts, DB, or remote loaders just impl `SkillLoader`.

---

## 12. MCP (`mcp.rs`) — client mode default

```rust
pub enum McpMode {
    Client,   // DEFAULT — we host the MCP client (stdio/http), wrap into our tools
    Native,   // opt-in — pass remote-URL server to provider; provider executes server-side
}

#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServer {
    Stdio { command: String, #[serde(default)] args: Vec<String>,
            #[serde(default)] env: HashMap<String, String> },   // Client only
    Http  { url: String, #[serde(default)] headers: HashMap<String, String> },  // Client or Native
    Sse   { url: String, #[serde(default)] headers: HashMap<String, String> },
}
pub struct McpConfig { #[serde(rename = "mcpServers")] pub servers: HashMap<String, McpServer> }
```

**Client mode (default, wraps MCP into our tool system):** connect → `list_tools` (returns
JSON Schema, already our `ToolDef` shape) → wrap each as `McpToolDyn: ToolDyn<Ctx>` namespaced
`mcp__<server>__<tool>` → register as a `ToolSet`. We execute locally and return normal
`tool_result`. Wins: stdio + http, private/local servers, **`can_use_tool` applies**, any
provider, dynamic refresh on `list_changed` (atomic registry slice swap), large servers use
deferred + `tool_search`. This is how OpenAI Agents SDK and Claude Agent SDK both work.

**Native mode (opt-in):** merges a remote https server into `ChatRequest` as provider-specific
config (Anthropic `mcp_servers` + `mcp_toolset`, beta `mcp-client-2025-11-20`; OpenAI Responses
`{type:"mcp",...}`). Provider lists + executes server-side; `mcp_tool_use`/`mcp_tool_result`
(Anthropic) or `mcp_call` (OpenAI) arrive in the response. Caveats: remote-URL only (no stdio),
no `can_use_tool` interception, only Claude / OpenAI-Responses.

---

## 13. Caching (`cache.rs`) — maximize token savings

```rust
pub enum CacheStrategy {
    Off,
    Auto,                                                       // breakpoint on last cacheable block
    Breakpoints { ttl: CacheTtl, points: Vec<CacheTarget> },    // Tools|System|LastUserMsg|MessageIndex
}
pub enum CacheTtl { FiveMin, OneHour }
```

We do **not** build our own token cache — both vendors cache server-side. Strategy:
- **Stable, append-only prefix** (tools → system → skills → early turns) so the prefix stays
  cached every loop step.
- Anthropic serializer places `cache_control` breakpoints (max 4; serializer caps + warns).
  OpenAI is automatic (no-op), but `cached_tokens` still flows into `Usage.cache_read_tokens`,
  so savings are observable uniformly.
- **Compaction** in `prepare_step`: when near a token threshold, summarize older turns into one
  synthetic message, keep last N exchanges + live tool pairs, re-apply a fresh breakpoint, emit
  `LoopEvent::Compacted`. Persistent rules live in `system` (re-sent + auto-cached), never in the
  first user message (may be compacted away).

---

## 14. Errors (`error.rs`)

```rust
pub enum ProviderError { Transport(/* http err */), Deserialize { err: String, body: String },
                         Provider { status: u16, kind: String, message: String },
                         InvalidArgument(String), Cancelled }
pub enum ToolError { /* user tool failures; carries message + is_error */ }
pub enum AiError { Provider(ProviderError), Tool(ToolError), Schema(/* validation */), Mcp(/* ... */) }
```

`thiserror` typed errors. Always carry the raw provider body for drift debugging.

---

## 15. Open caveats to resolve before/while implementing

1. **Long-running / human-in-the-loop tools.** If a consumer routes `on_ask` through a slow
   transport, that's the consumer's timeout to own. The SDK's own tool execution must support a
   **configurable per-tool timeout** and a **progress channel** (`ToolCx` emit) rather than a
   single fixed blocking deadline.
2. **OpenAI two surfaces.** Ship `OpenAiResponsesModel` (default) + `OpenAiChatModel`. Map both
   usage field namings into the single `Usage`.
3. **Thinking round-trip.** Preserve `AssistantContent::Thinking` losslessly; replay verbatim
   (with signature) for Anthropic or it 400s. For OpenAI keep in `raw` / rely on stateful chaining.
4. **YAML dep for skills.** `serde_yaml` is unmaintained — pick a maintained parser.
5. **MCP client crate.** Pick a maintained Rust MCP client (stdio + streamable-http).

---

## 16. Build order (high level)

1. `model`: `JsonSchema` trait + `emit_jsonschema.rs` + `Field.description` IR slot (+ tests).
2. `ai-sdk` core types: `message`, `usage`, `error`, `cache`.
3. `Provider` trait + `ClaudeModel` (first provider) with complete + stream.
4. `tool` (Tool/ToolDyn/registry/ToolSet) + `ai-sdk-derive` `#[tool]` macro.
5. `agent` loop (event stream, stop_when, can_use_tool, prepare_step, cancel, inject).
6. `tool_search` + deferred registry.
7. `OpenAiResponsesModel` + `OpenAiChatModel`.
8. `mcp` client mode, then native mode.
9. `skill` (`SkillLoader` + `FsSkillLoader` + built-in `load_skill`/`read_skill_resource`/`search_skills` tools, 3-level progressive disclosure).
10. `examples/` incl. a stakit-router integration showing client-tools via `Ctx`.

Quality gate every step: `./code-check.sh` (fmt + clippy -D warnings + build + nextest + doctests).

---

## 17. Implementation status (built + verified)

All layers below are implemented in `crates/ai-sdk` (+ `crates/ai-sdk-derive`,
`crates/model` JSON Schema) and pass the full gate (`./code-check.sh`: rustfmt,
clippy `pedantic`+`nursery` `-D warnings`, build, nextest, doctests) with
**no `unsafe`** (forbidden workspace-wide). ~50 unit/integration tests, plus a
live e2e suite that passes against **real Claude + OpenAI**.

| Layer | Module(s) | Status |
|---|---|---|
| JSON Schema | `model::json_schema`, `model-derive::emit_jsonschema` | ✅ `#[derive(JsonSchema)]`, `///`+`#[arg]` docs |
| Core types | `message`, `usage`, `cache`, `error` | ✅ unified model, `Usage`/`Pricing`, `CacheStrategy` |
| Providers | `provider`, `provider::claude`, `provider::openai` | ✅ Claude + OpenAI: complete + streaming SSE, whole-`ToolCall` accumulation, usage/cost |
| Tools + macro | `tool`, `ai-sdk-derive` | ✅ `Tool`/`ToolDyn`/`TypedTool`/`ToolRegistry`/`ToolSet`, `#[tool]` (4 sig shapes) |
| Agent loop | `agent`, `loop_event`, `cancel`, `cx` | ✅ event stream, `can_use_tool`→`on_ask`, `CancelToken`, `prepare_step`, `run_with_input` injection, per-step usage/cost, `stop_when` |
| Tool search | `tool` (deferred + search) + loop | ✅ `register_deferred`, built-in `tool_search` |
| Context loaders | `context` | ✅ `ContextLoader` (multi-source), `FsContextLoader` |
| Skills | `skill` + loop | ✅ `SkillLoader`/`FsSkillLoader`, frontmatter parse, progressive disclosure, `load_skill`/`search_skills` |
| MCP | `mcp` | ✅ config parse + `${VAR}` expansion, `McpTransport` trait, `McpToolSet` (client-mode, namespaced) |
| Parallel tools | `agent` loop | ✅ concurrent `join_all` (default; deterministic barrier test) |
| Reasoning | `provider` `ThinkingConfig` / `extra` | ✅ Claude `thinking` budget; OpenAI `reasoning_effort` via `extra` |
| Examples + e2e | `examples/`, `tests/e2e.rs` | ✅ `chat` + `weather_agent`; live e2e (`#[ignore]`) — see matrix |

### Live e2e coverage (both providers unless noted)

`e2e_claude_all` + `e2e_openai_all` each assert, against the real API: tool
round-trip, **multi-step agentic loop**, **parallel tool calls**, **skill
loading** (`load_skill`), **prompt injection** (`run_with_input`), and **tool
approval** (`can_use_tool` deny → error result). **Both** providers also have
live **prompt caching** (`cache_read_tokens > 0` on the 2nd call — Anthropic
explicit breakpoint, OpenAI automatic) and **streaming** text-delta tests.
Parallel-execution concurrency is additionally proven offline with a barrier
deadlock test.

### Deltas from the sketch (all deliberate)

- **`ContextLoader`** added (multi-source context loading: file/DB/HTTP/RAG),
  merged into system + seed history before the loop. Same loader pattern as skills.
- **`TypedTool<T>` wrapper** instead of a blanket `impl ToolDyn for T: Tool` —
  Rust coherence forbids the blanket alongside concrete `ToolDyn` impls (MCP).
  `register()` wraps automatically.
- **MCP** is a `McpTransport` trait + adapter (client-mode, any transport plugs
  in: `rmcp`, custom). The concrete `rmcp`-backed transport + MCP `Native`
  passthrough are the remaining follow-up (gated behind a future `mcp` feature);
  config parsing, namespacing and tool-wrapping are done and tested.
- **`schema` is opt-in** on `model`/`model-derive` (pulls `serde_json`) so plain
  validation/TS users don't pay for it; ai-sdk enables it.

### Running

```bash
./code-check.sh                                              # full offline gate
cargo nextest run -p stakit-ai-sdk --run-ignored all -E 'test(e2e)'   # live e2e (.env keys)
cargo run -p stakit-ai-sdk --example weather_agent          # live demo (from repo root)
```
```
