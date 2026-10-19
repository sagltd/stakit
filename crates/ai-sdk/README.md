# stakit-ai-sdk

Provider-agnostic primitives for building LLM agents in Rust. Not a fixed agent —
a toolbox: a `Provider` abstraction (Claude + OpenAI built in, bring your own),
a typed tool system with a `#[tool]` macro, an injectable + cancellable agent
loop exposed as an event stream, pluggable skill and context loaders, MCP config,
and per-step token-usage + cost telemetry.

- **Provider-agnostic** — Claude and OpenAI ship built in; add any backend by
  implementing one small trait.
- **Typed tools** — arguments derive their JSON Schema; no hand-written schemas.
- **Injectable loop** — inject user messages mid-run, cancel in-flight calls.
- **Pluggable everything** — providers, tools, skills, context, MCP transports
  are all traits with reference impls.
- **Telemetry** — token usage and estimated cost per step.
- **No `unsafe`** (forbidden workspace-wide).

The crate depends on nothing from a web framework or router — integration with a
host happens through the agent's generic context type `Ctx`.

## Install

```toml
[dependencies]
stakit-ai-sdk = { version = "0.1", features = ["claude", "openai"] }
stakit-model  = { version = "0.1", features = ["schema"] } # for #[derive(Model, JsonSchema)] on tool args
serde         = { version = "1", features = ["derive"] }
serde_json    = "1"
tokio         = { version = "1", features = ["macros", "rt-multi-thread"] }
futures       = "0.3"
```

Features: `claude` (default), `openai` (default). Disable defaults and pick to
compile only the provider you use.

## Quick start

```rust
use futures::StreamExt;
use stakit_ai_sdk::{Agent, AgentEvent, ClaudeClient, Message};

#[tokio::main]
async fn main() {
    let client = ClaudeClient::from_env().expect("ANTHROPIC_API_KEY");
    let provider = client.model("claude-haiku-5-5");
    let mut agent = Agent::new(())
        .provider(provider)
        .with_context(vec![Message::user("Say hello in five words.")]);

    let mut run = agent.run();
    while let Some(event) = run.next().await {
        match event {
            AgentEvent::MessageDelta(t) => print!("{t}"),
            AgentEvent::Done(out) =>
                println!("\n[{} in / {} out]", out.usage.input_tokens, out.usage.output_tokens),
            _ => {}
        }
    }
}
```

`OpenAiClient` uses the same interface: `OpenAiClient::from_env()` and
`client.model("gpt-6-luna")`. Provider handles can coexist:

```rust,ignore
let mut agent = Agent::new(())
    .provider(claude.model("claude-sonnet-5-5"))
    .register_provider(openai.model("gpt-6-luna"))
    .model("gpt-6-luna")
    .with_context(vec![Message::user("Say hello.")]);
let result = agent.run().await?;
```

Choose the provider/model per task. Signed provider history remains bound to its
original request context; switching a model does not make opaque reasoning from
another provider replayable. See [the provider contract](docs/provider-contract.md)
for implementing and verifying an adapter without changing application tools.

## The loop & events

`agent.run()` returns an `AgentRun` which is both a `Stream<Item = AgentEvent>` and
an `IntoFuture<Output = Result<Outcome, AgentError>>`:

```rust
pub enum AgentEvent {
    StepStart  { index: u32 },
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCall   { id: String, name: String, args: Value },
    ToolResult { id: String, name: String, result: ToolOutcome },
    StepEnd    { index: u32, text: String, reasoning: Option<String>, usage: Usage, cost: Option<f64> },
    Done(Outcome),
}
```

The loop: call model → stream output → run any requested tools (concurrently) →
append results → repeat, until the model ends its turn, a middleware stops, or the
default step cap (`StepCountIs(16)`) fires. Budget / custom stop conditions are a
[`AgentMiddleware`] concern (return `Flow::Stop` from `on_step`).

## Tools

### With the `#[tool]` macro

Argument type derives `Model` + `JsonSchema` — the JSON Schema sent to the model
is generated from it, with `///` doc-comments becoming parameter descriptions.

```rust
use stakit_ai_sdk::{tool, ToolError};
use stakit_model::{Model, JsonSchema};

#[derive(serde::Deserialize, Model, JsonSchema)]
struct WeatherArgs {
    /// City name, e.g. "Paris"
    #[validate(min_len = 1)]
    city: String,
}

/// Get the current weather for a city.   ← becomes the tool description
#[tool]
async fn get_weather(args: WeatherArgs) -> Result<String, ToolError> {
    Ok(format!("21°C and sunny in {}.", args.city))
}

let agent = Agent::new(()).provider(provider).register_tool(get_weather);
```

Accepted `#[tool]` signatures (sync or async): `(cx, args)`, `(args)`, `(cx)`, `()`.
Override name/description: `#[tool(name = "weather", description = "…")]`.

### Tools with context

`Ctx` is your world (DB handle, websocket client, auth). Tools receive `&ToolCx<Ctx>`:

```rust
struct App { db: Db }

#[tool]
async fn lookup(cx: &ToolCx<App>, args: LookupArgs) -> Result<Row, ToolError> {
    Ok(cx.ctx().db.get(&args.id).await?)   // `?` works on any std error
}

let mut agent = Agent::new(App { db }).provider(provider).register_tool(lookup);
let _ = agent.run().await;
```

This is also how **client-side tools** and **human approval** wire in — through
your `Ctx`, with no dependency on any transport.

### Hand-written tools (no macro)

```rust
use stakit_ai_sdk::{Tool, ToolCx, ToolError, BoxFuture};

struct Reverse;
impl Tool<()> for Reverse {
    type Args = ReverseArgs;       // : Deserialize + JsonSchema + Validate
    type Output = String;
    fn name(&self) -> &'static str { "reverse" }
    fn description(&self) -> &'static str { "Reverse a string" }
    fn run<'a>(&'a self, _cx: &'a ToolCx<()>, args: ReverseArgs)
        -> BoxFuture<'a, Result<String, ToolError>>
    {
        Box::pin(async move { Ok(args.text.chars().rev().collect()) })
    }
}
```

### Parallel tool calls

When the model requests several tools in one turn, the loop runs them
**concurrently** (`join_all`) and returns all results in one turn. Enabled by
default (both APIs allow parallel tool use unless told otherwise).

## Add your own provider

Implement the `Provider` trait using only the public API — no fork needed.

```rust
use futures::StreamExt;
use stakit_ai_sdk::{
    event_stream, ChatRequest, ChatResponse, EventStream, Provider, ProviderError,
    StopReason, StreamEvent, Usage,
};

#[derive(Clone)]
struct MyProvider { model: String, /* http client, key, … */ }

impl Provider for MyProvider {
    fn model_id(&self) -> &str { &self.model }

    fn complete(&self, req: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        // 1. map `req` (system, messages, tools, tool_choice, cache, thinking) to your wire format
        // 2. POST, parse response into unified `AssistantContent` blocks
        Box::pin(async move {
            Ok(ChatResponse {
                content: vec![/* AssistantContent::Text(...) / ToolUse {...} */],
                stop: StopReason::EndTurn,
                // These example counts stand for observed final wire counters.
                usage: Usage { input_tokens: 10, output_tokens: 5, ..Usage::default() },
            })
        })
    }

    fn stream(&self, _req: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        // map your SSE/byte stream to `StreamEvent`s; `event_stream(...)` builds it:
        Box::pin(async move {
            Ok(event_stream(vec![
                Ok(StreamEvent::TextDelta("hi".into())),
                // Emit End only after all content and observed final counters.
                Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Usage { input_tokens: 10, output_tokens: 5, ..Usage::default() },
                }),
            ]))
        })
    }
}

// use it exactly like the built-ins:
let mut agent = Agent::new(()).provider(MyProvider { model: "my-model".into() });
```

Map your provider's usage into the unified `Usage`
(`input_tokens` / `output_tokens` / `cache_create_tokens` / `cache_read_tokens` /
`reasoning_tokens`) so cost telemetry works. `cache_create_1h_tokens` is a
subset of cache writes. Ordinary input excludes cache reads and writes, including
when a vendor's raw prompt count includes them.

A successful `ChatResponse` or terminal `StreamEvent::End` must carry observed,
final mandatory input and output counters for the entire response. The numbers
in the example stand for counters parsed from a backend response; do not copy
literal counts or substitute `Usage::default()` for missing evidence. An explicit
zero is valid only when the backend actually reported zero. Missing or partial
mandatory counters must produce `ProviderError`, even if earlier progress
contained usable counters. Preserve that progress without claiming completion.
Final counters need not all be repeated in the last wire frame: an adapter may
retain earlier observed input when its protocol establishes that input covers
the whole response. Final output must cover the last emitted content; an earlier
partial output tally cannot replace final evidence.

Emit `End` only after all content blocks close and its final tally covers every
emitted block. No content or tool calls may follow it. `ToolCall` events queue
calls; the agent must not execute them before a valid `End`. EOF without `End`
is failure. These obligations apply to custom providers as well as built-ins;
the numeric `Usage` type cannot verify the origin or finality of its counters.

## Structured extraction with usage

`LLM::extract_with_usage::<T>()` returns `Extraction<T>`: the exact selected
request `model`, `usage: Option<Usage>`, and `result: Result<T, ExtractionError>`.
`T` implements `DeserializeOwned + JsonSchema + Validate` (`#[derive(Model)]`
provides validation). It shares request fitting and actual `extract` tool-result
checks with `extract`, additionally requiring exactly one tool-use block and
validating the decoded value. No retries or extra AI
calls are made. `LLM::cache(CacheStrategy)` uses the same model-aware cache
preflight as the agent; the default remains `Auto`.

Record observed usage against the returned model **before inspecting/applying
`result`**, including invalid payloads, validation failures, missing tools,
refusals and incomplete output. `Some(Usage::zero())` is an observed normalized
zero. `None` means no tally was returned: preflight errors made no call, while a
provider error may have incurred cost. Do not turn unknown usage into zero.
The current unary provider-error contract cannot preserve counters embedded in
malformed wire responses. Dropping/cancelling the future before it returns also
produces no accounting outcome; background job timeout/cancellation accounting
still needs an explicit host policy.

The new errors contain fixed categories, no provider/body/serde/validation text;
`Extraction` debug formatting redacts the extracted value. Existing `extract`
keeps its return type, bounds and legacy error behavior; use the new API for
privacy-sensitive metered jobs. Neither helper implements a job queue, retries,
provenance gates or memory learning. See `LLM::extract_with_usage` rustdoc for an
accounting-before-result example.

## Tool approval (human-in-the-loop)

Gate every tool call by implementing `AgentMiddleware::on_tool_approve`:

```rust
struct GuardMiddleware;

#[async_trait]
impl AgentMiddleware<MyCtx> for GuardMiddleware {
    async fn on_tool_approve(&self, cx: &AgentCx<MyCtx>, call: &PendingToolCall)
        -> Result<Approval, AgentError>
    {
        if is_dangerous(&call.name) {
            // Ask the user via the app context; halt the run if they say no.
            if cx.ctx().client.confirm(&call.name, &call.args).await {
                Ok(Approval::Allow)
            } else {
                Ok(Approval::Stop { message: Some("user rejected".into()) })
            }
        } else {
            Ok(Approval::Allow)
        }
    }
}
```

`Deny { message }` feeds the reason to the model as a tool result and lets the loop
continue. `Stop` halts the entire run.

## Skills (progressive disclosure)

Only skill `name` + `description` enter the system prompt; full bodies load on
demand via the built-in `load_skill` / `search_skills` tools. Implement `SkillLoader`
to source skills from anywhere — a database, a folder, a remote API:

```rust
#[async_trait]
impl SkillLoader<MyCtx> for MyLoader {
    async fn list(&self, ctx: &MyCtx) -> Result<Vec<Skill>, AgentError> {
        Ok(ctx.db.list_skills().await?)
    }
    async fn load(&self, ctx: &MyCtx, id: &str) -> Result<SkillContent, AgentError> {
        let body = ctx.db.load_skill(id).await?;
        Ok(SkillContent { body, references: vec![] })
    }
}

let mut agent = Agent::new(ctx).provider(provider).skills(MyLoader);
```

## Conversation load / save

Seed the conversation and persist it via `AgentMiddleware::on_start` /
`on_step_done` — no separate context-loader trait:

```rust
#[async_trait]
impl AgentMiddleware<ReqCtx> for DbConversation {
    async fn on_start(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        let prior = cx.ctx().db.load(&cx.ctx().session).await?;
        cx.messages_mut().splice(0..0, prior);
        Ok(Flow::Continue)
    }
    async fn on_step_done(&self, cx: &mut AgentCx<ReqCtx>) -> Result<Flow, AgentError> {
        cx.ctx().db.save(&cx.ctx().session, cx.messages()).await?;
        Ok(Flow::Continue)
    }
}
```

## MCP (client mode)

Parse standard `mcpServers` JSON config (stdio + http/sse, `${VAR}` / `${VAR:-default}`
expansion), connect via an `McpTransport`, and register the server's tools as a
`ToolSet` (namespaced `mcp__<server>__<tool>`):

```rust
let cfg = McpConfig::from_json(json)?.expand_env();
let set = McpToolSet::connect("web", my_transport).await?;   // my_transport: impl McpTransport
agent.register_tool_set(set);
```

`McpTransport` is the seam — plug in `rmcp` or any client. (The concrete
`rmcp`-backed transport is a follow-up phase.)

## Caching & cost

- **Caching**: keep a stable, append-only prefix. `CacheStrategy::Auto` uses
  the adapter's available caching. Providers declare their implemented controls
  through `cache_capabilities_for`; unsupported explicit breakpoints/TTLs fail
  before transport. `Off` suppresses explicit markers without promising to turn
  off implicit provider caching. A cache hit at one provider cannot be reused
  by another. Reads and writes surface in the same `Usage` contract.
- **Usage**: input, cache reads and cache writes are disjoint. One-hour writes
  (`cache_create_1h_tokens`) are a subset of all writes, never extra input.
  Set `ModelPrice::cache_write_1h` when offering that lifetime. An observed hourly
  write without a configured rate makes the estimated cost unknown.
- **Pricing**: sum each request's original model/tier cost. Do not apply prompt
  tiers to the run's aggregate tokens; that would reprice earlier requests.
- **Cost**: set a `Pricing` table; each step's `AgentEvent::StepEnd` carries the
  estimated dollar cost (a client-side estimate — don't bill from it).

```rust
.pricing(Pricing::new().with("claude-haiku-4-5-20251001",
    ModelPrice { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: 1.25, cache_write_1h: Some(2.0) })) // $/1M tokens
```

## Reasoning / extended thinking

- Claude: `.thinking(ThinkingConfig::Budget(2048))` → Anthropic `thinking`.
- OpenAI: `reasoning_effort` via the request `extra` passthrough (reasoning models only).

## Examples

```bash
cargo run -p stakit-ai-sdk --example chat            # streaming chat
cargo run -p stakit-ai-sdk --example weather_agent   # tool + skills + approval + cost
```

Both load keys from a repo-root `.env` (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`).

## Testing

```bash
cargo nextest run -p stakit-ai-sdk                                    # offline unit/integration
cargo nextest run -p stakit-ai-sdk --run-ignored all -E 'test(e2e)'  # live e2e (needs .env keys)
```

Live e2e covers, for **both** Claude and OpenAI: tools, multi-step loop, parallel
tool calls, skill loading, prompt injection, tool approval, prompt caching, and
streaming.

## License

MIT OR Apache-2.0
