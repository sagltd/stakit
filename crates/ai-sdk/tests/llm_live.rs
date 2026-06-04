//! Live per-provider extraction tests for [`stakit_ai_sdk::LLM::extract`].
//!
//! Gated behind `--features live-tests`. Each test skips gracefully when the
//! required API key is absent, so CI stays offline.
//!
//! Run with:
//! ```bash
//! cargo nextest run -p stakit-ai-sdk --features live-tests -E 'test(llm_live)'
//! ```
#![cfg(feature = "live-tests")]

#[cfg(feature = "claude")]
use stakit_ai_sdk::ClaudeClient;
use stakit_ai_sdk::LLM;
#[cfg(feature = "openai")]
use stakit_ai_sdk::OpenAiClient;
use stakit_model::{JsonSchema, Model};

// ── Test subject ─────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, Model, JsonSchema)]
#[allow(dead_code)]
struct User {
    /// The user's name.
    name: String,
    /// The user's age.
    age: u32,
}

// ── Claude live test ──────────────────────────────────────────────────────────

#[cfg(feature = "claude")]
#[tokio::test]
async fn claude_llm_extract_user() {
    let Ok(_) = std::env::var("ANTHROPIC_API_KEY") else {
        return;
    };

    let client = ClaudeClient::from_env().expect("ANTHROPIC_API_KEY must be set");
    let provider = client.model("claude-haiku-4-5-20251001");

    let user: User = LLM::new(provider)
        .system("Extract the user from the text.")
        .user("Bob is 30 years old.")
        .extract::<User>()
        .await
        .expect("LLM::extract must succeed with a valid Claude key");

    assert_eq!(user.name, "Bob", "extracted name must be 'Bob'");
    assert_eq!(user.age, 30, "extracted age must be 30");
}

// ── OpenAI live test ──────────────────────────────────────────────────────────

#[cfg(feature = "openai")]
#[tokio::test]
async fn openai_llm_extract_user() {
    let Ok(_) = std::env::var("OPENAI_API_KEY") else {
        return;
    };

    let client = OpenAiClient::from_env().expect("OPENAI_API_KEY must be set");
    let provider = client.model("gpt-4o-mini");

    let user: User = LLM::new(provider)
        .system("Extract the user from the text.")
        .user("Bob is 30 years old.")
        .extract::<User>()
        .await
        .expect("LLM::extract must succeed with a valid OpenAI key");

    assert_eq!(user.name, "Bob", "extracted name must be 'Bob'");
    assert_eq!(user.age, 30, "extracted age must be 30");
}
