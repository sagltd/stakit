//! Offline tests for the skill system: list, inject, search, load-on-demand,
//! no-eager-load guarantee, and unknown-id error path.
//!
//! These tests use a mock `SkillLoader` (no network) and a recording provider
//! that captures the `ChatRequest.system` text sent to the provider.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use stakit_ai_sdk::{
    Agent, AgentCx, AgentError, AgentMiddleware, Flow, Message, Skill, SkillContent, SkillLoader,
    ToolOutcome,
};

#[path = "support/mock.rs"]
mod mock;
use mock::{Scripted, assert_tool_error, last_tool_result, text_turn};

// ── Mock skill loader ────────────────────────────────────────────────────────

struct MockLoader {
    skills: Vec<Skill>,
    load_calls: Arc<AtomicU32>,
}

impl MockLoader {
    fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            load_calls: Arc::new(AtomicU32::new(0)),
        }
    }

    const fn with_counter(skills: Vec<Skill>, counter: Arc<AtomicU32>) -> Self {
        Self {
            skills,
            load_calls: counter,
        }
    }
}

#[async_trait::async_trait]
impl SkillLoader<()> for MockLoader {
    async fn list(&self, _ctx: &()) -> Result<Vec<Skill>, AgentError> {
        Ok(self.skills.clone())
    }

    async fn load(&self, _ctx: &(), id: &str) -> Result<SkillContent, AgentError> {
        self.load_calls.fetch_add(1, Ordering::SeqCst);
        self.skills.iter().find(|s| s.id == id).map_or_else(
            || Err(AgentError::Skill(format!("unknown skill id: {id}"))),
            |s| {
                Ok(SkillContent {
                    body: format!("body-of-{}", s.name),
                    references: vec![],
                })
            },
        )
    }
}

// ── Recording provider ───────────────────────────────────────────────────────
//
// `Scripted` captures the full ChatRequest for each call so tests can assert
// on `system.text`, tool definitions, etc.

/// Provider that always responds with a single text turn ("done").
fn text_only() -> Scripted {
    Scripted::turns("recorder", |_| text_turn("done"))
}

/// Provider that produces a tool call on step 0, then "done" on step 1.
fn with_tool_call(name: &'static str, args: &'static str) -> Scripted {
    Scripted::tool_then_text("recorder", name, args, "done")
}

// ── Test 1: list() is called at run start and injected into system prompt ────

#[tokio::test]
async fn list_is_called_at_start_and_injected_into_system_prompt() {
    let skills = vec![
        Skill {
            id: "s1".into(),
            name: "Skill One".into(),
            description: "first skill".into(),
        },
        Skill {
            id: "s2".into(),
            name: "Skill Two".into(),
            description: "second skill".into(),
        },
    ];
    let provider = text_only();
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .skills(MockLoader::new(skills))
        .with_context(vec![Message::user("hi")]);

    let _ = agent.run().await.expect("outcome");

    let captured = provider.requests();
    assert!(!captured.is_empty(), "provider must have been called");
    let system = captured[0]
        .system
        .as_ref()
        .expect("system must be set when skills are loaded");
    let text = &*system.text;

    // The manifest must include the skill names and ids.
    assert!(
        text.contains("Skill One"),
        "system prompt must list skill names"
    );
    assert!(
        text.contains("Skill Two"),
        "system prompt must list skill names"
    );
    assert!(
        text.contains("s1"),
        "system prompt must include skill id s1"
    );
    assert!(
        text.contains("s2"),
        "system prompt must include skill id s2"
    );
    assert!(
        text.contains("first skill"),
        "system prompt must include descriptions"
    );
    assert!(
        text.contains("second skill"),
        "system prompt must include descriptions"
    );
}

// ── Test 2: search_skills built-in returns full-text matches ─────────────────

#[tokio::test]
async fn search_skills_returns_matches_for_query() {
    let skills = vec![
        Skill {
            id: "s1".into(),
            name: "Rust coding".into(),
            description: "write Rust".into(),
        },
        Skill {
            id: "s2".into(),
            name: "Python scripting".into(),
            description: "write Python".into(),
        },
    ];
    // A provider that calls `search_skills` on the first turn.
    let provider =
        Scripted::tool_then_text("searcher", "search_skills", r#"{"query":"rust"}"#, "done");
    let mut agent = Agent::new(())
        .provider(provider)
        .model("searcher")
        .skills(MockLoader::new(skills))
        .with_context(vec![Message::user("search")]);

    let result = match last_tool_result(agent.run()).await {
        ToolOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {other:?}"),
    };

    let matches = result["matches"]
        .as_array()
        .expect("matches must be an array");
    // Only "Rust coding" should match the query "rust".
    assert_eq!(matches.len(), 1, "only the Rust skill should match");
    assert_eq!(matches[0]["id"], "s1");
    assert_eq!(matches[0]["name"], "Rust coding");
}

// ── Test 3a: load_skill calls loader.load and returns the body ───────────────

#[tokio::test]
async fn load_skill_returns_body_on_known_id() {
    let skills = vec![Skill {
        id: "rust".into(),
        name: "Rust guide".into(),
        description: "guides for Rust".into(),
    }];
    let provider = with_tool_call("load_skill", r#"{"id":"rust"}"#);
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .skills(MockLoader::new(skills))
        .with_context(vec![Message::user("load it")]);

    let result = match last_tool_result(agent.run()).await {
        ToolOutcome::Ok(v) => v,
        other => panic!("expected Ok, got {other:?}"),
    };

    assert_eq!(
        result["body"], "body-of-Rust guide",
        "body must be the loader's response"
    );
}

// ── Test 3b: load_skill with unknown id → ToolOutcome::Error (not a panic) ───

#[tokio::test]
async fn load_skill_with_unknown_id_yields_error_not_panic() {
    let skills = vec![Skill {
        id: "known".into(),
        name: "Known Skill".into(),
        description: "a skill".into(),
    }];
    let provider = with_tool_call("load_skill", r#"{"id":"unknown-id"}"#);
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .skills(MockLoader::new(skills))
        .with_context(vec![Message::user("load unknown")]);

    assert_tool_error(agent.run(), "unknown skill id: unknown-id").await;
}

// ── Test 3c: load_skill with missing/empty id → ToolOutcome::Error ───────────

#[tokio::test]
async fn load_skill_with_empty_id_yields_error() {
    let skills = vec![Skill {
        id: "s1".into(),
        name: "S1".into(),
        description: "a skill".into(),
    }];
    let provider = with_tool_call("load_skill", r"{}");
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .skills(MockLoader::new(skills))
        .with_context(vec![Message::user("load empty")]);

    assert_tool_error(agent.run(), "missing required argument: id").await;
}

// ── Test 4: loader.load is NOT called before a load_skill tool call ──────────

#[tokio::test]
async fn loader_load_not_called_eagerly() {
    let load_counter = Arc::new(AtomicU32::new(0));
    let skills = vec![Skill {
        id: "s1".into(),
        name: "Skill One".into(),
        description: "a skill".into(),
    }];
    // Provider that just emits text — no load_skill tool call.
    let provider = text_only();
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .skills(MockLoader::with_counter(skills, Arc::clone(&load_counter)))
        .with_context(vec![Message::user("hi")]);

    let _ = agent.run().await.expect("outcome");

    assert_eq!(
        load_counter.load(Ordering::SeqCst),
        0,
        "loader.load must not be called when no load_skill tool call was made"
    );
}

// ── Test 5: manifest is cached (injected every step) AND set_system still works

/// Sets the base system prompt to "UPDATED" once `index >= 1`.
struct SetSystemAtStep1;

#[async_trait::async_trait]
impl AgentMiddleware<()> for SetSystemAtStep1 {
    async fn on_step(&self, cx: &mut AgentCx<'_, ()>) -> Result<Flow, AgentError> {
        if cx.index() >= 1 {
            cx.set_system("UPDATED");
        }
        Ok(Flow::Continue)
    }
}

#[tokio::test]
async fn skill_manifest_is_injected_each_step_and_set_system_still_applies() {
    let skills = vec![Skill {
        id: "s1".into(),
        name: "Skill One".into(),
        description: "first skill".into(),
    }];
    // Step 0 calls a built-in skill tool so the loop runs a second step; the
    // middleware then swaps the base system prompt before step 1.
    let provider = with_tool_call("search_skills", r#"{"query":"x"}"#);
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("recorder")
        .skills(MockLoader::new(skills))
        .register_middleware(SetSystemAtStep1)
        .with_context(vec![Message::user("hi")]);

    let _ = agent.run().await.expect("outcome");

    let captured = provider.requests();
    assert!(captured.len() >= 2, "expected at least two provider calls");

    // The manifest (cached once) must be injected on BOTH steps.
    for (i, req) in captured.iter().enumerate() {
        let text = &*req
            .system
            .as_ref()
            .unwrap_or_else(|| panic!("step {i} must have a system prompt"))
            .text;
        assert!(
            text.contains("Skill One"),
            "step {i} system must include the cached skill manifest: {text}"
        );
    }

    // set_system must still take effect on the next step: step 0 has no base
    // prompt, step 1 carries the middleware's "UPDATED" base before the manifest.
    assert!(
        !captured[0]
            .system
            .as_ref()
            .unwrap()
            .text
            .contains("UPDATED"),
        "step 0 must not yet have the updated base prompt"
    );
    assert!(
        captured[1]
            .system
            .as_ref()
            .unwrap()
            .text
            .contains("UPDATED"),
        "step 1 must reflect set_system from on_step"
    );
}
