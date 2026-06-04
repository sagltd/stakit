//! Offline prompt-cache tests: build provider request bodies and assert on the
//! resulting JSON (no network).
//!
//! These prove the scale-correct caching behavior from the design (§12):
//! Claude `Auto` breakpoint placement (tools → system → a rolling previous-turn
//! breakpoint), the `OpenAI` `prompt_cache_key`, and — critically — that the
//! shared tools+system prefix is byte-identical across users and stable as a
//! conversation grows, so many concurrent users keep hitting one cached prefix.
#![cfg(all(feature = "claude", feature = "openai"))]

use serde_json::{Value, json};
use stakit_ai_sdk::{
    CacheStrategy, ChatRequest, Message, SystemPrompt, ToolDef, UserContent,
    test_support::{claude_body, openai_body},
};

const MODEL: &str = "claude-opus-4-8";

/// Builds `n` distinct tool definitions with stable, deterministic shapes.
fn tools(n: usize) -> Vec<ToolDef> {
    (0..n)
        .map(|i| {
            ToolDef::new(
                format!("tool_{i}"),
                format!("description for tool {i}"),
                json!({
                    "type": "object",
                    "properties": { "arg": { "type": "string" } },
                    "required": ["arg"],
                }),
            )
        })
        .collect()
}

/// A two-turn history: user → assistant → user (the trailing user message is
/// the in-progress turn).
fn two_turn_history(first_user: &str, trailing_user: &str) -> Vec<Message> {
    vec![
        Message::user(first_user),
        Message::Assistant(vec![stakit_ai_sdk::AssistantContent::Text(
            "sure, here is an answer".into(),
        )]),
        Message::user(trailing_user),
    ]
}

/// Builds a Claude request with `n` tools, a system prompt, and `messages`.
fn claude_request(n: usize, messages: Vec<Message>) -> ChatRequest {
    let mut req = ChatRequest::new(MODEL);
    req.system = Some(SystemPrompt::from(
        "You are a helpful assistant.\n\n## Available skills\n- foo (foo): does foo",
    ));
    req.tools = tools(n);
    req.messages = messages;
    req
}

/// Counts `cache_control` breakpoints anywhere in a JSON value.
fn count_cache_control(v: &Value) -> usize {
    match v {
        Value::Object(map) => {
            let here = usize::from(map.contains_key("cache_control"));
            here + map.values().map(count_cache_control).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

/// Whether the last tool in the body carries a `cache_control` breakpoint.
fn last_tool_is_cached(body: &Value) -> bool {
    body["tools"]
        .as_array()
        .and_then(|t| t.last())
        .is_some_and(|t| t.get("cache_control").is_some())
}

/// Whether the system block carries a `cache_control` breakpoint (system is an
/// array of text blocks when cached).
fn system_is_cached(body: &Value) -> bool {
    body["system"]
        .as_array()
        .and_then(|s| s.first())
        .is_some_and(|b| b.get("cache_control").is_some())
}

// 1. Claude `Auto` places breakpoints after tools, after system, plus a rolling
//    one on the previous turn; the total is within Anthropic's [1, 4] cap.
#[test]
fn claude_auto_breakpoints_after_tools_and_system_and_rolling() {
    let req = claude_request(3, two_turn_history("first question", "follow-up question"));
    let body = claude_body(&req);

    let count = count_cache_control(&body);
    assert!(
        (2..=4).contains(&count),
        "expected 2..=4 breakpoints, got {count}: {body:#}"
    );
    assert!(last_tool_is_cached(&body), "tools block must be cached");
    assert!(system_is_cached(&body), "system block must be cached");

    // The rolling breakpoint sits on the previous turn's boundary — the
    // second-to-last user message (index 0 here), not the trailing user turn.
    let msgs = body["messages"].as_array().expect("messages array");
    assert!(
        msgs[0]["content"]
            .as_array()
            .and_then(|c| c.last())
            .is_some_and(|b| b.get("cache_control").is_some()),
        "rolling breakpoint must be on the previous turn's last block: {body:#}"
    );
    assert!(
        msgs.last()
            .and_then(|m| m["content"].as_array())
            .and_then(|c| c.last())
            .is_some_and(|b| b.get("cache_control").is_none()),
        "the in-progress (last) turn must NOT be cached: {body:#}"
    );
}

// 2. `OpenAI` `Auto` routes the conversation to a cache shard via
//    `prompt_cache_key`, taken from the request's `cache_key`.
#[test]
fn openai_auto_sets_prompt_cache_key() {
    let mut req = ChatRequest::new("gpt-4o");
    req.system = Some(SystemPrompt::from("be brief"));
    req.messages = vec![Message::user("hi")];
    req.cache_key = Some("sess-1".into());

    let body = openai_body(&req);
    assert_eq!(body["prompt_cache_key"], "sess-1");

    // Absent key → no field (route by content alone).
    req.cache_key = None;
    let body = openai_body(&req);
    assert!(
        body.get("prompt_cache_key").is_none(),
        "no key must omit prompt_cache_key"
    );
}

// 3. Prefix stability across users: identical tools+system, but a different
//    trailing user message and a different cache key, must serialize to a
//    byte-identical tools+system prefix (so both users hit the same cache).
#[test]
fn claude_shared_prefix_identical_across_users() {
    let mut a = claude_request(3, vec![Message::user("user A question")]);
    a.cache_key = Some("user-a".into());
    let mut b = claude_request(
        3,
        vec![Message::user("a totally different question from B")],
    );
    b.cache_key = Some("user-b".into());

    let body_a = claude_body(&a);
    let body_b = claude_body(&b);

    // The trailing messages differ...
    assert_ne!(body_a["messages"], body_b["messages"]);
    // ...but the cached shared prefix (tools + system, including the
    // cache_control breakpoints) is byte-for-byte identical.
    assert_eq!(
        serde_json::to_string(&body_a["tools"]).unwrap(),
        serde_json::to_string(&body_b["tools"]).unwrap(),
        "tools prefix must be byte-identical across users"
    );
    assert_eq!(
        serde_json::to_string(&body_a["system"]).unwrap(),
        serde_json::to_string(&body_b["system"]).unwrap(),
        "system prefix must be byte-identical across users"
    );
    // Sanity: the shared prefix actually carries breakpoints to hit.
    assert!(last_tool_is_cached(&body_a) && system_is_cached(&body_a));
}

// 4. Conversation continuity: appending a new turn must not move the tools or
//    system breakpoints — turn N+1 keeps hitting the same earlier prefix.
#[test]
fn claude_breakpoints_stable_across_turns() {
    let one_turn = claude_request(3, vec![Message::user("first question")]);
    let two_turn = claude_request(
        3,
        two_turn_history("first question", "second, follow-up question"),
    );

    let body1 = claude_body(&one_turn);
    let body2 = claude_body(&two_turn);

    assert_eq!(
        serde_json::to_string(&body1["tools"]).unwrap(),
        serde_json::to_string(&body2["tools"]).unwrap(),
        "tools breakpoint must not move when a turn is appended"
    );
    assert_eq!(
        serde_json::to_string(&body1["system"]).unwrap(),
        serde_json::to_string(&body2["system"]).unwrap(),
        "system breakpoint must not move when a turn is appended"
    );

    // The newer turn additionally gains a rolling breakpoint that the
    // single-turn request did not have.
    assert_eq!(count_cache_control(&body1), 2, "one-turn: tools + system");
    assert_eq!(
        count_cache_control(&body2),
        3,
        "two-turn: tools + system + rolling"
    );
}

// Bonus: `Off` places no breakpoints at all (regression guard).
#[test]
fn claude_off_places_no_breakpoints() {
    let mut req = claude_request(3, two_turn_history("q1", "q2"));
    req.cache = CacheStrategy::Off;
    let body = claude_body(&req);
    assert_eq!(
        count_cache_control(&body),
        0,
        "Off must not cache: {body:#}"
    );
}

// Bonus: an in-turn user message with a tool result still receives the rolling
// breakpoint on its last block (covers the non-text trailing block path).
#[test]
fn claude_rolling_breakpoint_on_tool_result_turn() {
    // user(q) → assistant(tool_use) → user(tool_result) → assistant → user(q2)
    let messages = vec![
        Message::user("question"),
        Message::Assistant(vec![stakit_ai_sdk::AssistantContent::ToolUse {
            id: "t1".into(),
            name: "tool_0".into(),
            input: json!({ "arg": "x" }),
        }]),
        Message::User(vec![UserContent::ToolResult {
            id: "t1".into(),
            content: vec![stakit_ai_sdk::ToolResultPart::Text("result".into())],
            is_error: false,
        }]),
        Message::Assistant(vec![stakit_ai_sdk::AssistantContent::Text("done".into())]),
        Message::user("another question"),
    ];
    let req = claude_request(2, messages);
    let body = claude_body(&req);

    // Second-to-last user message is the tool-result turn (index 2).
    let msgs = body["messages"].as_array().unwrap();
    assert!(
        msgs[2]["content"]
            .as_array()
            .and_then(|c| c.last())
            .is_some_and(|b| b.get("cache_control").is_some()),
        "rolling breakpoint must land on the previous-turn tool-result block: {body:#}"
    );
}
