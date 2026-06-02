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
use stakit_ai_sdk::{Agent, CancelToken, ClaudeClient, LoopEvent, Message};

#[tokio::main]
async fn main() {
    let client = ClaudeClient::from_env().expect("ANTHROPIC_API_KEY");
    let agent = Agent::<_, ()>::builder(client.model("claude-haiku-4-5-20251001"))
        .build();

    let mut stream = Box::pin(agent.run(
        vec![Message::user_text("Say hello in five words.")],
        (),                  // your context (here: nothing)
        CancelToken::new(),
    ));

    while let Some(event) = stream.next().await {
        match event {
            LoopEvent::TextDelta(t) => print!("{t}"),
            LoopEvent::Done { usage, cost, .. } =>
                println!("\n[{} in / {} out, ~${:.6}]", usage.input_tokens, usage.output_tokens, cost.unwrap_or(0.0)),
            _ => {}
        }
    }
}
```

The model is named **once** (on the provider handle). `OpenAiClient` is identical:
`OpenAiClient::from_env()` + `.model("gpt-4o-mini")`.

## The loop & events

`agent.run(history, ctx, cancel)` returns a `Stream<Item = LoopEvent>`:

```rust
pub enum LoopEvent {
    StepStart { step: u32 },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall   { id: String, name: String, input: Value },
    ToolResult { id: String, output: Value, is_error: bool },
    Usage      { step: u32, usage: Usage, cost: Option<f64> }, // per-step tokens + $
    StepEnd    { step: u32, stop: StopReason },
    Done       { text: String, usage: Usage, cost: Option<f64>, reason: FinishReason },
}
```

The loop: call model → stream output → run any requested tools (concurrently) →
append results → repeat, until the model ends its turn or a `stop_when` matches.

Stop conditions (OR-ed; default `StepCountIs(20)`):

```rust
.stop_when(vec![StopCond::StepCountIs(10), StopCond::BudgetUsd(0.50), StopCond::HasToolCall("done".into())])
```

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

let agent = Agent::<_, ()>::builder(provider).register(get_weather).build();
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

let agent = Agent::<_, App>::builder(provider).register(lookup).build();
agent.run(history, App { db }, CancelToken::new());
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

### Add / remove tools at runtime

The registry is internally synchronized — mutate a live (even cloned) agent:

```rust
agent.register_tool(get_weather);
agent.register_tool_set(my_bundle);   // any `ToolSet` (e.g. an MCP server)
agent.remove_tool("get_weather");
let names: Vec<String> = agent.tool_names();
```

### Parallel tool calls

When the model requests several tools in one turn, the loop runs them
**concurrently** (`join_all`) and returns all results in one turn. Enabled by
default (both APIs allow parallel tool use unless told otherwise).

### Tool search at scale

Register large tool sets as **deferred** — withheld from the prompt until the
built-in `tool_search` tool surfaces them (Anthropic-style, provider-agnostic):

```rust
.register_deferred(big_tool)   // name+desc not sent until searched
```

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
    type Raw = serde_json::Value;             // native body, kept on ChatResponse.raw

    fn model_id(&self) -> &str { &self.model } // agent reads the model from here

    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse<Self::Raw>, ProviderError> {
        // 1. map `req` (system, messages, tools, tool_choice, cache, thinking) to your wire format
        // 2. POST, parse response into unified `AssistantContent` blocks
        Ok(ChatResponse {
            content: vec![/* AssistantContent::Text(...) / ToolUse {...} */],
            stop: StopReason::EndTurn,
            usage: Usage { input_tokens: 10, output_tokens: 5, ..Usage::default() },
            raw: serde_json::Value::Null,
        })
    }

    async fn stream(&self, req: ChatRequest) -> Result<EventStream, ProviderError> {
        // map your SSE/byte stream to `StreamEvent`s, accumulating tool-call
        // argument fragments so a whole `ToolCall` is emitted once complete.
        // `event_stream(...)` builds the stream without a direct `futures` dep:
        Ok(event_stream(vec![
            Ok(StreamEvent::TextDelta("hi".into())),
            Ok(StreamEvent::End { stop: StopReason::EndTurn, usage: Usage::default() }),
        ]))
    }
}

// use it exactly like the built-ins:
let agent = Agent::<_, ()>::builder(MyProvider { model: "my-model".into() }).build();
```

Map your provider's usage into the unified `Usage`
(`input_tokens` / `output_tokens` / `cache_create_tokens` / `cache_read_tokens` /
`reasoning_tokens`) so cost telemetry works.

## Inject user input mid-loop

Hold the sender; send any time — drained at the next step boundary:

```rust
let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
let mut stream = Box::pin(agent.run_with_input(history, ctx, cancel.clone(), Some(rx)));

while let Some(ev) = stream.next().await {
    if let LoopEvent::StepEnd { .. } = ev {
        tx.send(Message::user_text("also check Tokyo")).ok(); // applied before next call
    }
}
```

`run(...)` is `run_with_input(..., None)`. Cancel mid-run with `cancel.cancel()`.

## Tool approval (human-in-the-loop)

`can_use_tool` runs before every tool call; `Ask` routes to `on_ask` (e.g. a
human via your `Ctx`):

```rust
.can_use_tool(|name, args, _cx| Box::pin(async move {
    if is_dangerous(name) { Permission::Ask } else { Permission::Allow }
}))
.on_ask(|name, args, cx| Box::pin(async move {
    if cx.ctx().client.confirm(name, args).await { Permission::Allow }
    else { Permission::Deny { reason: "rejected".into() } }
}))
```

`Deny` synthesizes an `is_error` tool result (the tool never runs); the model sees
the reason and can recover.

## Skills (progressive disclosure)

Only skill `name` + `description` enter the prompt; full bodies load on demand via
the built-in `load_skill` / `search_skills` tools.

```rust
.skills(FsSkillLoader::new(".agents/skills"))   // <root>/<name>/SKILL.md
```

Bring your own source by implementing `SkillLoader` (DB, server, embedded):

```rust
impl SkillLoader<Ctx> for MyLoader {
    fn list<'a>(&'a self, cx: &'a ToolCx<Ctx>) -> BoxFuture<'a, Result<Vec<SkillManifest>, AiError>> { … }
    fn load<'a>(&'a self, name: &'a str, cx: &'a ToolCx<Ctx>) -> BoxFuture<'a, Result<SkillContent, AiError>> { … }
}
```

## Context loaders

Seed the system prompt / history from anywhere (file, DB, HTTP, RAG). Register
many — all run before the loop and merge:

```rust
.context_loader(FsContextLoader::new("PROMPT.md"))
.context_loader(MyRagLoader)        // impl ContextLoader<Ctx>
```

```rust
impl ContextLoader<Ctx> for MyRagLoader {
    fn load<'a>(&'a self, cx: &'a ToolCx<Ctx>) -> BoxFuture<'a, Result<LoadedContext, AiError>> {
        Box::pin(async move { Ok(LoadedContext { system: Some(text), messages: vec![] }) })
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

- **Caching**: keep a stable, append-only prefix; Anthropic gets explicit
  `cache_control` breakpoints, OpenAI auto-caches. Controlled by `CacheStrategy`
  (default `Auto`). Savings show up uniformly as `Usage::cache_read_tokens`.
- **Cost**: set a `Pricing` table; each step's `LoopEvent::Usage` carries the
  estimated dollar cost (a client-side estimate — don't bill from it).

```rust
.pricing(Pricing::new().with("claude-haiku-4-5-20251001",
    ModelPrice { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: 1.25 })) // $/1M tokens
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
