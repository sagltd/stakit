//! Explicit live checks for the `OpenAI` models offered by Elios.
#![cfg(all(feature = "openai", feature = "live-tests"))]

use stakit_ai_sdk::{
    AssistantContent, ChatRequest, LLM, Message, OpenAiClient, Provider, StopReason, SystemPrompt,
};

#[path = "support/live.rs"]
mod live;

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and paid network calls"]
async fn modern_luna_plain_completion() {
    let client = OpenAiClient::from_env().expect("live verification requires OPENAI_API_KEY");
    let text = LLM::new(client.model("gpt-6-luna"))
        .user("Reply with exactly the word READY.")
        .max_tokens(64)
        .text()
        .await
        .expect("Luna completion");
    assert_eq!(text.trim(), "READY");
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and paid network calls"]
async fn modern_sol_tool_round_trip() {
    let client = OpenAiClient::from_env().expect("live verification requires OPENAI_API_KEY");
    live::assert_status_round_trip(client.model("gpt-6-sol"), "silver fern 681").await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and two paid network calls"]
async fn modern_luna_reports_real_cache_usage() {
    let client = OpenAiClient::from_env().expect("live verification requires OPENAI_API_KEY");
    let provider = client.model("gpt-6-luna");
    let mut request = ChatRequest::new("gpt-6-luna");
    request.system = Some(SystemPrompt::from(format!(
        "For this synthetic cache check reply only READY. Reference material:\n{}",
        "The synthetic fixture has four records: cedar, maple, birch, and oak. ".repeat(200)
    )));
    request.messages = vec![Message::user("Reply READY.")];
    request.max_tokens = 64;
    request.cache_key = Some("elios-release-cache-check-v1".into());
    let first = provider
        .complete(request.clone())
        .await
        .expect("cache write request");
    let second = provider
        .complete(request)
        .await
        .expect("cache reuse request");
    for response in [&first, &second] {
        assert_eq!(response.stop, StopReason::EndTurn);
        assert!(response.content.iter().any(|block| matches!(block,
            AssistantContent::Text(text) if text.trim() == "READY"
        )));
        assert!(response.usage.total_input() >= 1024);
        assert!(response.usage.output_tokens > 0);
    }
    // A hit depends on routing/capacity; demonstrate real cache accounting
    // without promising that the next request always hits the same cache.
    assert!(
        first.usage.cache_create_tokens > 0
            || first.usage.cache_read_tokens > 0
            || second.usage.cache_create_tokens > 0
            || second.usage.cache_read_tokens > 0,
        "neither request reported cache reads or writes"
    );
    eprintln!(
        "first cache usage: {:?}; second: {:?}",
        first.usage, second.usage
    );
}
