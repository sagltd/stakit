//! Live end-to-end tests against the real Claude and `OpenAI` APIs.
//!
//! Marked `#[ignore]` so the normal gate stays deterministic and offline. Run
//! them explicitly (keys loaded from the repo-root `.env`):
//!
//! ```bash
//! cargo nextest run -p stakit-ai-sdk --run-ignored all -E 'test(e2e)'
//! ```
//!
//! `e2e_claude_all` and `e2e_openai_all` each exercise the same scenarios —
//! tools, multi-step loop, parallel tool calls, skill loading, prompt
//! injection, and tool approval (middleware `Deny`) — against the real provider.
#![cfg(all(feature = "claude", feature = "openai"))]
#![allow(dead_code)]

use futures::StreamExt;
use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentEvent, AgentMiddleware, Approval, CacheStrategy, ChatRequest,
    ClaudeClient, Finish, Flow, Message, ModelPrice, OpenAiClient, PendingToolCall, Pricing,
    Provider, Skill, SkillContent, SkillLoader, SystemPrompt, ToolOutcome, tool,
};
use stakit_model::{JsonSchema, Model};

const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
const OPENAI_MODEL: &str = "gpt-4o-mini";

#[derive(serde::Deserialize, Model, JsonSchema)]
struct WeatherArgs {
    /// City name, e.g. "Paris"
    #[validate(min_len = 1)]
    city: String,
}

/// Get the current weather for a city.
#[tool]
async fn get_weather(args: WeatherArgs) -> Result<String, stakit_ai_sdk::ToolError> {
    Ok(format!("It is 21°C and sunny in {}.", args.city))
}

fn load_env() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    let _ = dotenvy::from_path(path);
}

fn pricing(model: &str) -> Pricing {
    Pricing::new().with(
        model,
        ModelPrice {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write: 1.25,
        },
    )
}

/// A trivial in-test skill loader (replaces the removed `FsSkillLoader`).
struct DemoSkills;

#[async_trait::async_trait]
impl SkillLoader<()> for DemoSkills {
    async fn list(&self, _ctx: &()) -> Result<Vec<Skill>, AgentError> {
        Ok(vec![Skill {
            id: "rust-best-practices".into(),
            name: "rust-best-practices".into(),
            description: "Idiomatic Rust guidance.".into(),
        }])
    }
    async fn load(&self, _ctx: &(), id: &str) -> Result<SkillContent, AgentError> {
        if id == "rust-best-practices" {
            Ok(SkillContent {
                body: "Write idiomatic Rust: prefer borrows, handle errors with Result.".into(),
                references: Vec::new(),
            })
        } else {
            Err(AgentError::Skill(format!("unknown skill: {id}")))
        }
    }
}

/// A middleware that denies a named tool (replaces `can_use_tool`).
struct DenyPolicy {
    tool: &'static str,
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for DenyPolicy {
    async fn on_tool_approve(
        &self,
        _cx: &AgentCx<'_, ()>,
        call: &PendingToolCall,
    ) -> Result<Approval, AgentError> {
        if call.name == self.tool {
            Ok(Approval::Deny {
                message: format!("{} blocked by policy", call.name),
            })
        } else {
            Ok(Approval::Allow)
        }
    }
}

/// A middleware that injects one extra user turn before the first model call.
struct InjectQuestion {
    text: &'static str,
}

#[async_trait::async_trait]
impl AgentMiddleware<()> for InjectQuestion {
    async fn on_start(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        cx.messages_mut().push(Message::user(self.text));
        Ok(Flow::Continue)
    }
}

async fn collect(agent: &mut Agent<()>) -> Vec<AgentEvent> {
    agent.run().collect().await
}

fn done(events: &[AgentEvent]) -> (String, Finish) {
    match events.last() {
        Some(AgentEvent::Done(o)) => (o.text.clone(), o.finish.clone()),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn tool_calls(events: &[AgentEvent], name: &str) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { name: n, .. } if n == name))
        .count()
}

fn ok_tool_results(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolResult {
                    result: ToolOutcome::Ok(_),
                    ..
                }
            )
        })
        .count()
}

// --- scenarios (provider-agnostic) ----------------------------------------

/// Tool round-trip + multi-step loop + usage + cost.
async fn scenario_tools_and_loop<P: Provider + 'static>(model: P, id: &str) {
    let mut agent = Agent::new(())
        .provider(model)
        .model(id)
        .pricing(pricing(id))
        .max_tokens(512)
        .register_tool(get_weather)
        .with_context(vec![Message::user(
            "Use the get_weather tool for Paris, then tell me the weather in one sentence.",
        )]);
    let events = collect(&mut agent).await;

    assert!(
        tool_calls(&events, "get_weather") >= 1,
        "no tool call: {events:?}"
    );
    assert!(ok_tool_results(&events) >= 1, "no successful tool result");
    // tool turn + final turn => at least two steps.
    let steps = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::StepEnd { .. }))
        .count();
    assert!(steps >= 2, "expected a multi-step loop, got {steps}");
    let (text, finish) = done(&events);
    assert!(matches!(finish, Finish::EndTurn));
    assert!(!text.is_empty());
    let cost = match events.last() {
        Some(AgentEvent::Done(o)) => o.cost,
        _ => None,
    };
    assert!(cost.unwrap_or(0.0) > 0.0, "cost should be estimated");
}

/// The model issues several tool calls; the loop runs them concurrently.
async fn scenario_parallel_tools<P: Provider + 'static>(model: P, id: &str) {
    let mut agent = Agent::new(())
        .provider(model)
        .model(id)
        .max_tokens(512)
        .register_tool(get_weather)
        .with_context(vec![Message::user(
            "Call the get_weather tool separately for BOTH Paris and Tokyo, then summarize both.",
        )]);
    let events = collect(&mut agent).await;
    assert!(
        tool_calls(&events, "get_weather") >= 2,
        "expected >=2 weather tool calls: {events:?}"
    );
}

/// Skill loading via the built-in `load_skill` tool (progressive disclosure).
async fn scenario_skills<P: Provider + 'static>(model: P, id: &str) {
    let mut agent = Agent::new(())
        .provider(model)
        .model(id)
        .max_tokens(512)
        .system("You are a helpful assistant.")
        .skills(DemoSkills)
        .with_context(vec![Message::user(
            "Call load_skill for the \"rust-best-practices\" skill, then reply with the word loaded.",
        )]);
    let events = collect(&mut agent).await;
    assert!(
        tool_calls(&events, "load_skill") >= 1,
        "model should call load_skill: {events:?}"
    );
    assert!(
        ok_tool_results(&events) >= 1,
        "load_skill should succeed: {events:?}"
    );
}

/// Mid-loop prompt injection via a middleware.
async fn scenario_inject<P: Provider + 'static>(model: P, id: &str) {
    let mut agent = Agent::new(())
        .provider(model)
        .model(id)
        .max_tokens(64)
        .register_middleware(InjectQuestion {
            text: "What is 2 + 2? Reply with only the number.",
        })
        .with_context(vec![Message::user("Await a question.")]);
    let events = collect(&mut agent).await;
    let (text, _) = done(&events);
    assert!(
        text.contains('4'),
        "injected question should be answered: {text:?}"
    );
}

/// Tool approval: a middleware denies the call; the model gets an error result.
async fn scenario_approval_denies<P: Provider + 'static>(model: P, id: &str) {
    let mut agent = Agent::new(())
        .provider(model)
        .model(id)
        .max_tokens(256)
        .register_tool(get_weather)
        .register_middleware(DenyPolicy {
            tool: "get_weather",
        })
        .with_context(vec![Message::user(
            "Use the get_weather tool to get the weather in Paris.",
        )]);
    let events = collect(&mut agent).await;
    assert!(
        tool_calls(&events, "get_weather") >= 1,
        "model should attempt the tool"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { result: ToolOutcome::Denied { message }, .. }
                if message.contains("blocked by policy")
        )),
        "denied tool should yield a Denied result: {events:?}"
    );
}

// --- per-provider entry points --------------------------------------------

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn e2e_claude_all() {
    load_env();
    let Ok(client) = ClaudeClient::from_env() else {
        eprintln!("ANTHROPIC_API_KEY not set; skipping");
        return;
    };
    let m = || client.model(CLAUDE_MODEL);
    scenario_tools_and_loop(m(), CLAUDE_MODEL).await;
    scenario_parallel_tools(m(), CLAUDE_MODEL).await;
    scenario_skills(m(), CLAUDE_MODEL).await;
    scenario_inject(m(), CLAUDE_MODEL).await;
    scenario_approval_denies(m(), CLAUDE_MODEL).await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn e2e_openai_all() {
    load_env();
    let Ok(client) = OpenAiClient::from_env() else {
        eprintln!("OPENAI_API_KEY not set; skipping");
        return;
    };
    let m = || client.model(OPENAI_MODEL);
    scenario_tools_and_loop(m(), OPENAI_MODEL).await;
    scenario_parallel_tools(m(), OPENAI_MODEL).await;
    scenario_skills(m(), OPENAI_MODEL).await;
    scenario_inject(m(), OPENAI_MODEL).await;
    scenario_approval_denies(m(), OPENAI_MODEL).await;
}

// --- Claude-specific: caching + streaming ---------------------------------

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn e2e_claude_prompt_caching() {
    load_env();
    let Ok(client) = ClaudeClient::from_env() else {
        return;
    };
    let model = client.model(CLAUDE_MODEL);
    let big = "You are a meticulous assistant who always answers precisely. ".repeat(800);
    let request = || {
        let mut r = ChatRequest::new(CLAUDE_MODEL);
        r.system = Some(SystemPrompt {
            text: big.clone().into(),
            cache: true,
        });
        r.messages = vec![Message::user("Reply with the single word: ok")];
        r.max_tokens = 16;
        r.cache = CacheStrategy::Auto;
        r
    };
    let first = model.complete(request()).await.expect("first call");
    let second = model.complete(request()).await.expect("second call");
    assert!(
        first.usage.cache_create_tokens > 0 || second.usage.cache_read_tokens > 0,
        "expected cache activity: first={:?} second={:?}",
        first.usage,
        second.usage
    );
    assert!(
        second.usage.cache_read_tokens > 0,
        "second call should hit the prompt cache: {:?}",
        second.usage
    );
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn e2e_openai_prompt_caching() {
    load_env();
    let Ok(client) = OpenAiClient::from_env() else {
        return;
    };
    let model = client.model(OPENAI_MODEL);
    // OpenAI caches automatically over a large stable prefix (no markup).
    let big = "You are a meticulous assistant who always answers precisely. ".repeat(800);
    let request = || {
        let mut r = ChatRequest::new(OPENAI_MODEL);
        r.system = Some(SystemPrompt::from(big.clone()));
        r.messages = vec![Message::user("Reply with the single word: ok")];
        r.max_tokens = 16;
        r
    };
    // Prime, then read.
    let _ = model.complete(request()).await.expect("prime call");
    let second = model.complete(request()).await.expect("second call");
    assert!(
        second.usage.cache_read_tokens > 0,
        "second call should report cached tokens: {:?}",
        second.usage
    );
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn e2e_openai_streaming_text() {
    load_env();
    let Ok(client) = OpenAiClient::from_env() else {
        return;
    };
    let mut agent = Agent::new(())
        .provider(client.model(OPENAI_MODEL))
        .model(OPENAI_MODEL)
        .max_tokens(64)
        .with_context(vec![Message::user("Count from 1 to 5, words only.")]);
    let events = collect(&mut agent).await;
    let deltas = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageDelta(_)))
        .count();
    assert!(deltas > 0, "expected streamed text deltas: {events:?}");
    let (text, _) = done(&events);
    assert!(!text.is_empty());
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn e2e_claude_streaming_text() {
    load_env();
    let Ok(client) = ClaudeClient::from_env() else {
        return;
    };
    let mut agent = Agent::new(())
        .provider(client.model(CLAUDE_MODEL))
        .model(CLAUDE_MODEL)
        .max_tokens(64)
        .with_context(vec![Message::user("Count from 1 to 5, words only.")]);
    let events = collect(&mut agent).await;
    let deltas = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageDelta(_)))
        .count();
    assert!(deltas > 0, "expected streamed text deltas: {events:?}");
    let (text, _) = done(&events);
    assert!(!text.is_empty());
}
