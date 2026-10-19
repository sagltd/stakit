//! Explicit live release checks. Missing credentials fail instead of passing offline.
#![cfg(all(feature = "claude", feature = "live-tests"))]

use stakit_ai_sdk::{
    CacheStrategy, CacheTarget, CacheTtl, ChatRequest, ClaudeClient, LLM, Message, Provider,
    SystemPrompt,
};

#[path = "support/live.rs"]
mod live;

#[derive(Debug, serde::Deserialize, stakit_model::JsonSchema)]
struct Person {
    name: String,
}

#[tokio::test]
#[ignore = "real Haiku check: requires ANTHROPIC_API_KEY and paid network calls"]
async fn modern_haiku_plain_completion() {
    let client = ClaudeClient::from_env().expect("live verification requires ANTHROPIC_API_KEY");
    let text = LLM::new(client.model("claude-haiku-5-5"))
        .user("Reply with exactly the word READY.")
        .max_tokens(64)
        .text()
        .await
        .expect("Haiku completion");
    assert_eq!(text.trim(), "READY");
}

#[tokio::test]
#[ignore = "real Sonnet extraction: requires ANTHROPIC_API_KEY and paid network calls"]
async fn modern_sonnet_extraction_without_forced_tool_choice() {
    let client = ClaudeClient::from_env().expect("live verification requires ANTHROPIC_API_KEY");
    let person: Person = LLM::new(client.model("claude-sonnet-5-5"))
        .system("Extract only the person's name from the supplied text.")
        .user("The person's name is Dana.")
        .max_tokens(512)
        .extract()
        .await
        .expect("Sonnet structured extraction");
    assert_eq!(person.name, "Dana");
}

#[tokio::test]
#[ignore = "real Sonnet tool loop: requires ANTHROPIC_API_KEY and paid network calls"]
async fn modern_sonnet_tool_round_trip() {
    let client = ClaudeClient::from_env().expect("live verification requires ANTHROPIC_API_KEY");
    live::assert_status_round_trip(client.model("claude-sonnet-5-5"), "copper kite 742").await;
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and two paid synthetic cache calls"]
async fn modern_haiku_hourly_cache_accounting() {
    let client = ClaudeClient::from_env().expect("live verification requires ANTHROPIC_API_KEY");
    let provider = client.model("claude-haiku-5-5");
    let mut request = ChatRequest::new("claude-haiku-5-5");
    request.system = Some(SystemPrompt::from(format!(
        "For this synthetic cache check reply only READY. Reference material:\n{}",
        "The synthetic fixture has four records: cedar, maple, birch, and oak. ".repeat(400)
    )));
    request.messages = vec![Message::user("Reply READY.")];
    request.max_tokens = 64;
    request.cache = CacheStrategy::Breakpoints {
        ttl: CacheTtl::OneHour,
        points: vec![CacheTarget::System, CacheTarget::LastUserMessage],
    };
    let first = provider
        .complete(request.clone())
        .await
        .expect("hourly write request");
    let second = provider
        .complete(request)
        .await
        .expect("hourly reuse request");
    for response in [&first, &second] {
        assert_eq!(response.stop, stakit_ai_sdk::StopReason::EndTurn);
        assert!(response.usage.total_input() > 2048);
        assert_eq!(
            response.usage.cache_create_1h_tokens,
            response.usage.cache_create_tokens
        );
    }
    // A cache read may miss because of routing. Require real cache activity,
    // and verify every observed write belongs to the requested one-hour tier.
    assert!(
        first.usage.cache_create_1h_tokens > 0
            || first.usage.cache_read_tokens > 0
            || second.usage.cache_create_1h_tokens > 0
            || second.usage.cache_read_tokens > 0
    );
    eprintln!(
        "first hourly cache usage: {:?}; second: {:?}",
        first.usage, second.usage
    );
}
