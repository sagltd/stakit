//! Retry + per-attempt timeout tests for the agent loop.
//!
//! Three scenarios:
//! 1. `transient_then_succeeds` — a `FlakyProvider` fails twice then succeeds;
//!    the run reaches `Finish::EndTurn`.
//! 2. `hung_provider_trips_timeout` — a `HangingProvider` never resolves;
//!    `with_timeout(5s)` + `start_paused` causes the test to terminate.
//! 3. `fatal_4xx_not_retried` — a 400-error provider is called exactly once
//!    (no retry on fatal errors).

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use stakit_ai_sdk::{
    Agent, ChatRequest, ChatResponse, EventStream, Finish, Message, Provider, ProviderError,
    StopReason, StreamEvent, Usage, event_stream,
};

// ── FlakyProvider: transient failures for the first N calls ──────────────────

struct FlakyProvider {
    calls: Arc<AtomicU32>,
    fail_times: u32,
}

impl Provider for FlakyProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "flaky"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = n < self.fail_times;
        Box::pin(async move {
            if fail {
                return Err(ProviderError::Transport("transient error".into()));
            }
            Ok(event_stream(vec![
                Ok(StreamEvent::TextDelta("recovered".into())),
                Ok(StreamEvent::End {
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ]))
        })
    }
}

// ── HangingProvider: stream() future never resolves ──────────────────────────

struct HangingProvider;

impl Provider for HangingProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "hanging"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async {
            // This future never resolves — the per-attempt timeout must cut it.
            futures::future::pending::<()>().await;
            unreachable!()
        })
    }
}

// ── FatalProvider: always returns 400 ────────────────────────────────────────

struct FatalProvider {
    calls: Arc<AtomicU32>,
}

impl Provider for FatalProvider {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "trait method must return &str"
    )]
    fn model_id(&self) -> &str {
        "fatal"
    }

    fn complete(&self, _r: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }

    fn stream(&self, _r: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ProviderError::Api {
                status: 400,
                kind: "invalid_request_error".into(),
                message: "bad request".into(),
            })
        })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// A `FlakyProvider` that fails on the first 2 calls then succeeds should
/// recover within `.with_retries(3)` and reach `Finish::EndTurn`.
#[tokio::test]
async fn transient_then_succeeds() {
    let calls = Arc::new(AtomicU32::new(0));
    let mut agent = Agent::new(())
        .provider(FlakyProvider {
            calls: Arc::clone(&calls),
            fail_times: 2,
        })
        .model("flaky")
        .with_retries(3)
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert_eq!(out.text, "recovered");
    assert!(
        matches!(out.finish, Finish::EndTurn),
        "expected EndTurn after retry recovery, got {:?}",
        out.finish
    );
    // 2 failures + 1 success = 3 total calls.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected 3 calls (2 failures + 1 success)"
    );
}

/// A provider whose `stream` future never resolves must be cut by the
/// per-attempt timeout. The agent is configured with a very short
/// per-attempt timeout (50 ms); an outer wall-clock `tokio::time::timeout`
/// of 10 s guards against the test itself hanging if timeout is broken.
///
/// Because `tokio/test-util` is not in dev-dependencies we cannot use
/// `start_paused = true` — the test uses real wall-clock time with a small
/// timeout so it stays fast (~100 ms).
#[tokio::test]
async fn hung_provider_trips_timeout() {
    let mut agent = Agent::new(())
        .provider(HangingProvider)
        .model("hanging")
        .with_retries(1)
        .with_timeout(Duration::from_millis(50))
        .with_context(vec![Message::user("hi")]);

    // Outer guard: if the per-attempt timeout is not applied the provider
    // would hang forever, and this outer timeout catches that (fast failure).
    let out = tokio::time::timeout(Duration::from_secs(10), agent.run())
        .await
        .expect("test timed out — per-attempt timeout was not applied")
        .expect("outcome");

    // The run must terminate with an error/stopped finish, not EndTurn.
    assert!(
        !matches!(out.finish, Finish::EndTurn),
        "expected an error/stopped finish when provider hangs, got EndTurn"
    );
}

/// A `ProviderError::Api { status: 400 }` is classified as fatal and must NOT
/// be retried. The provider's `stream` must be called exactly once.
#[tokio::test]
async fn fatal_4xx_not_retried() {
    let calls = Arc::new(AtomicU32::new(0));
    let mut agent = Agent::new(())
        .provider(FatalProvider {
            calls: Arc::clone(&calls),
        })
        .model("fatal")
        .with_retries(5) // high cap to show retries never fire
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Stopped for a fatal error, got {:?}",
        out.finish
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "fatal 4xx must not be retried — expected exactly 1 call"
    );
}

/// A provider that fails more times than allowed retries should stop after
/// `max_retries + 1` attempts.
#[tokio::test]
async fn retries_exhausted_stops_after_cap() {
    let calls = Arc::new(AtomicU32::new(0));
    let mut agent = Agent::new(())
        .provider(FlakyProvider {
            calls: Arc::clone(&calls),
            fail_times: 10, // always fails
        })
        .model("flaky")
        .with_retries(2)
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert!(
        matches!(out.finish, Finish::Stopped { .. }),
        "expected Stopped after retries exhausted, got {:?}",
        out.finish
    );
    // 1 initial attempt + 2 retries = 3 total calls.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "expected exactly 3 calls (1 + 2 retries)"
    );
}
