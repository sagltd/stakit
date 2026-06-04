#![cfg(feature = "live-tests")]
//! Live prompt-cache checks. Skipped (return early) when API keys are absent.
//!
//! Run with:
//! ```bash
//! cargo nextest run -p stakit-ai-sdk --features live-tests -E 'test(cache)'
//! ```

#[cfg(feature = "claude")]
use stakit_ai_sdk::ClaudeClient;
#[cfg(feature = "openai")]
use stakit_ai_sdk::OpenAiClient;
use stakit_ai_sdk::{Agent, Message};

/// A stable system fragment, repeated `.repeat(6)` at each call site so the
/// cached prefix clears the model's minimum cacheable size: 1024 tokens for
/// Claude Sonnet/Opus and `OpenAI`, but **2048 tokens for Claude Haiku** (the cheap
/// model used here). A prefix below the model's threshold is silently not cached.
const LARGE_SYSTEM: &str = "You are a helpful, knowledgeable AI assistant specializing in \
    software engineering, system design, data structures, algorithms, and computer science \
    fundamentals. When answering questions, you provide thorough explanations with concrete \
    examples. You understand that good software is readable, maintainable, testable, and \
    correct. You follow best practices such as SOLID principles, DRY, YAGNI, and clean code \
    conventions. You are familiar with multiple programming languages including Rust, Python, \
    TypeScript, Go, Java, C++, and more. You can explain complex topics like concurrency, \
    distributed systems, networking, cryptography, compiler theory, and operating systems. \
    You are patient and thorough, always willing to break down complex topics into simpler \
    parts. You cite tradeoffs when comparing approaches. You are aware of common pitfalls \
    and anti-patterns. You value correctness above all else, and you always verify your \
    reasoning before providing an answer. You have extensive knowledge of databases, caching \
    strategies, message queues, microservices, container orchestration, cloud platforms, and \
    DevOps practices. You understand HTTP, REST, GraphQL, gRPC, WebSockets, and other \
    network protocols. You can help with debugging, performance optimization, security \
    auditing, and code review. You follow a Socratic method when helping people learn, \
    encouraging them to think through problems rather than just giving answers. You are \
    familiar with agile methodologies, test-driven development, continuous integration, and \
    continuous deployment pipelines. You understand the tradeoffs between different \
    architectural patterns like monoliths, microservices, event-driven architectures, and \
    serverless. You can reason about time complexity, space complexity, and the practical \
    performance implications of algorithmic choices. You understand memory management, \
    garbage collection, reference counting, and manual memory management across different \
    language runtimes. You have knowledge of machine learning fundamentals, neural networks, \
    transformers, and large language models. You can help with data engineering, ETL \
    pipelines, data warehousing, and analytics. You are a reliable, accurate, and \
    comprehensive assistant for any technical question.";

#[cfg(feature = "claude")]
#[tokio::test]
async fn claude_second_call_reads_cache() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }

    let client = ClaudeClient::from_env().expect("ANTHROPIC_API_KEY must be set");
    // Use a model that supports prompt caching. Verified via raw API: sonnet-4-5
    // writes/reads the cache; `claude-haiku-4-5` did NOT cache on this account
    // even for a hand-built request, so it is unsuitable for a caching assertion.
    let model = "claude-sonnet-4-5";

    // First call — primes the cache.
    let mut agent = Agent::new(())
        .provider(client.model(model))
        .system(LARGE_SYSTEM.repeat(6))
        .max_tokens(64)
        .with_context(vec![Message::user("What is 2 + 2?")]);
    let out1 = agent.run().await.expect("first call");

    // Second call — should read from cache.
    let mut agent2 = Agent::new(())
        .provider(client.model(model))
        .system(LARGE_SYSTEM.repeat(6))
        .max_tokens(64)
        .with_context(vec![Message::user("What is 2 + 2?")]);
    let out2 = agent2.run().await.expect("second call");

    assert!(
        out2.usage.cache_read_tokens > 0,
        "expected cache_read_tokens > 0 on second call, got usage={:?}",
        out2.usage
    );
    // Sanity: both calls produced some output.
    assert!(!out1.text.is_empty(), "first call produced no text");
    assert!(!out2.text.is_empty(), "second call produced no text");
}

#[cfg(feature = "openai")]
#[tokio::test]
async fn openai_second_call_reports_cached_tokens() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }

    let client = OpenAiClient::from_env().expect("OPENAI_API_KEY must be set");
    // gpt-4o-mini supports prompt caching on long repeated prefixes.
    let model = "gpt-4o-mini";

    // First call — primes the cache.
    let mut agent = Agent::new(())
        .provider(client.model(model))
        .system(LARGE_SYSTEM.repeat(6))
        .max_tokens(64)
        .with_context(vec![Message::user("What is 2 + 2?")]);
    let _out1 = agent.run().await.expect("first call");

    // Second call — should read from cache.
    let mut agent2 = Agent::new(())
        .provider(client.model(model))
        .system(LARGE_SYSTEM.repeat(6))
        .max_tokens(64)
        .with_context(vec![Message::user("What is 2 + 2?")]);
    let out2 = agent2.run().await.expect("second call");

    assert!(
        out2.usage.cache_read_tokens > 0,
        "expected cache_read_tokens > 0 on second call, got usage={:?}",
        out2.usage
    );
}
