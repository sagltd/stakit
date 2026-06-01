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
//! injection, and tool approval (`can_use_tool`) — against the real provider.
#![cfg(all(feature = "claude", feature = "openai"))]
#![allow(dead_code)]

use futures::StreamExt;
use stakit_ai_sdk::{
    Agent, CacheStrategy, CancelToken, ChatRequest, ClaudeClient, FinishReason, FsSkillLoader,
    LoopEvent, Message, ModelPrice, OpenAiClient, Permission, Pricing, Provider, SystemPrompt,
    tool,
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

fn skills_root() -> String {
    format!("{}/../../.agents/skills", env!("CARGO_MANIFEST_DIR"))
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

async fn collect<P: Provider>(agent: &Agent<P, ()>, history: Vec<Message>) -> Vec<LoopEvent> {
    agent.run(history, (), CancelToken::new()).collect().await
}

fn done(events: &[LoopEvent]) -> (&str, FinishReason) {
    match events.last() {
        Some(LoopEvent::Done { text, reason, .. }) => (text, *reason),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn tool_calls(events: &[LoopEvent], name: &str) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, LoopEvent::ToolCall { name: n, .. } if n == name))
        .count()
}

// --- scenarios (provider-agnostic) ----------------------------------------

/// Tool round-trip + multi-step loop + usage + cost.
async fn scenario_tools_and_loop<P: Provider>(model: P, id: &str) {
    let agent = Agent::<P, ()>::builder(model)
        .model(id)
        .pricing(pricing(id))
        .max_tokens(512)
        .register(get_weather)
        .build();
    let events = collect(
        &agent,
        vec![Message::user_text(
            "Use the get_weather tool for Paris, then tell me the weather in one sentence.",
        )],
    )
    .await;

    assert!(
        tool_calls(&events, "get_weather") >= 1,
        "no tool call: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            LoopEvent::ToolResult {
                is_error: false,
                ..
            }
        )),
        "no tool result"
    );
    // tool turn + final turn => at least two steps.
    let steps = events
        .iter()
        .filter(|e| matches!(e, LoopEvent::StepEnd { .. }))
        .count();
    assert!(steps >= 2, "expected a multi-step loop, got {steps}");
    let (text, reason) = done(&events);
    assert_eq!(reason, FinishReason::EndTurn);
    assert!(!text.is_empty());
    let total: Option<f64> = events
        .iter()
        .find_map(|e| match e {
            LoopEvent::Done { cost, .. } => Some(*cost),
            _ => None,
        })
        .flatten();
    assert!(total.unwrap_or(0.0) > 0.0, "cost should be estimated");
}

/// The model issues several tool calls; the loop runs them concurrently.
async fn scenario_parallel_tools<P: Provider>(model: P, id: &str) {
    let agent = Agent::<P, ()>::builder(model)
        .model(id)
        .max_tokens(512)
        .register(get_weather)
        .build();
    let events = collect(
        &agent,
        vec![Message::user_text(
            "Call the get_weather tool separately for BOTH Paris and Tokyo, then summarize both.",
        )],
    )
    .await;
    assert!(
        tool_calls(&events, "get_weather") >= 2,
        "expected >=2 weather tool calls: {events:?}"
    );
}

/// Skill loading via the built-in `load_skill` tool (progressive disclosure).
async fn scenario_skills<P: Provider>(model: P, id: &str) {
    let agent = Agent::<P, ()>::builder(model)
        .model(id)
        .max_tokens(512)
        .skills(FsSkillLoader::new(skills_root()))
        .build();
    let events = collect(
        &agent,
        vec![Message::user_text(
            "Call load_skill for the \"rust-best-practices\" skill, then reply with the word loaded.",
        )],
    )
    .await;
    assert!(
        tool_calls(&events, "load_skill") >= 1,
        "model should call load_skill: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            LoopEvent::ToolResult {
                is_error: false,
                ..
            }
        )),
        "load_skill should succeed"
    );
}

/// Mid-loop prompt injection via `run_with_input`.
async fn scenario_inject<P: Provider>(model: P, id: &str) {
    let agent = Agent::<P, ()>::builder(model)
        .model(id)
        .max_tokens(64)
        .build();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(Message::user_text(
        "What is 2 + 2? Reply with only the number.",
    ))
    .unwrap();
    drop(tx);
    let events: Vec<LoopEvent> = agent
        .run_with_input(
            vec![Message::user_text("Await a question.")],
            (),
            CancelToken::new(),
            Some(rx),
        )
        .collect()
        .await;
    let (text, _) = done(&events);
    assert!(
        text.contains('4'),
        "injected question should be answered: {text:?}"
    );
}

/// Tool approval: `can_use_tool` denies the call; the model gets an error result.
async fn scenario_approval_denies<P: Provider>(model: P, id: &str) {
    let agent = Agent::<P, ()>::builder(model)
        .model(id)
        .max_tokens(256)
        .register(get_weather)
        .can_use_tool(|name, _args, _cx| {
            Box::pin(async move {
                Permission::Deny {
                    reason: format!("{name} blocked by policy"),
                }
            })
        })
        .build();
    let events = collect(
        &agent,
        vec![Message::user_text(
            "Use the get_weather tool to get the weather in Paris.",
        )],
    )
    .await;
    assert!(
        tool_calls(&events, "get_weather") >= 1,
        "model should attempt the tool"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            LoopEvent::ToolResult { is_error: true, output, .. }
                if output.as_str().is_some_and(|s| s.contains("blocked by policy"))
        )),
        "denied tool should yield an error result: {events:?}"
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
            text: big.clone(),
            cache: true,
        });
        r.messages = vec![Message::user_text("Reply with the single word: ok")];
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
        r.messages = vec![Message::user_text("Reply with the single word: ok")];
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
    let agent = Agent::<_, ()>::builder(client.model(OPENAI_MODEL))
        .model(OPENAI_MODEL)
        .max_tokens(64)
        .build();
    let events = collect(
        &agent,
        vec![Message::user_text("Count from 1 to 5, words only.")],
    )
    .await;
    let deltas = events
        .iter()
        .filter(|e| matches!(e, LoopEvent::TextDelta(_)))
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
    let agent = Agent::<_, ()>::builder(client.model(CLAUDE_MODEL))
        .model(CLAUDE_MODEL)
        .max_tokens(64)
        .build();
    let events = collect(
        &agent,
        vec![Message::user_text("Count from 1 to 5, words only.")],
    )
    .await;
    let deltas = events
        .iter()
        .filter(|e| matches!(e, LoopEvent::TextDelta(_)))
        .count();
    assert!(deltas > 0, "expected streamed text deltas: {events:?}");
    let (text, _) = done(&events);
    assert!(!text.is_empty());
}
