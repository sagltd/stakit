//! Retry + per-attempt timeout tests for the agent loop.
//!
//! Three scenarios:
//! 1. `transient_then_succeeds` — a flaky provider fails twice then succeeds;
//!    the run reaches `Finish::EndTurn`.
//! 2. `hung_provider_trips_timeout` — a hanging provider never resolves;
//!    `with_timeout(5s)` + `start_paused` causes the test to terminate.
//! 3. `fatal_4xx_not_retried` — a 400-error provider is called exactly once
//!    (no retry on fatal errors).

use std::time::Duration;

use futures::StreamExt as _;
use stakit_ai_sdk::{Agent, Finish, Message, ProviderError};

#[path = "support/mock.rs"]
mod mock;
use mock::{Scripted, assert_stopped, stream_of, text_turn};

// ── flaky: transient failures for the first N calls ──────────────────────────

fn flaky(fail_times: u32) -> Scripted {
    Scripted::new("flaky", move |call, _| {
        if call < fail_times {
            return Err(ProviderError::Transport("transient error".into()));
        }
        Ok(stream_of(text_turn("recovered")))
    })
}

// ── hanging: the stream never yields ─────────────────────────────────────────

fn hanging() -> Scripted {
    // The first event never arrives — the per-attempt timeout must cut it.
    Scripted::new("hanging", |_, _| Ok(futures::stream::pending().boxed()))
}

// ── fatal: always returns 400 ────────────────────────────────────────────────

fn fatal() -> Scripted {
    Scripted::new("fatal", |_, _| {
        Err(ProviderError::Api {
            status: 400,
            kind: "invalid_request_error".into(),
            message: "bad request".into(),
            retry_after: None,
        })
    })
}

// ── rate_limited_once: a 429 with Retry-After on the first call, then success ─

fn rate_limited_once(retry_after: Duration) -> Scripted {
    Scripted::new("ratelimited", move |call, _| {
        if call == 0 {
            return Err(ProviderError::Api {
                status: 429,
                kind: "rate_limit_error".into(),
                message: "slow down".into(),
                retry_after: Some(retry_after),
            });
        }
        Ok(stream_of(text_turn("recovered")))
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// A flaky provider that fails on the first 2 calls then succeeds should
/// recover within `.with_retries(3)` and reach `Finish::EndTurn`.
#[tokio::test]
async fn transient_then_succeeds() {
    let provider = flaky(2);
    let mut agent = Agent::new(())
        .provider(provider.clone())
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
        provider.calls(),
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
        .provider(hanging())
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
    let provider = fatal();
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("fatal")
        .with_retries(5) // high cap to show retries never fire
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert_stopped(&out);
    assert_eq!(
        provider.calls(),
        1,
        "fatal 4xx must not be retried — expected exactly 1 call"
    );
}

/// A 429 carrying `Retry-After` must delay the retry by at least that long.
///
/// The server asks for 600 ms — well above the default 250 ms first backoff —
/// so the loop must wait ~600 ms before the (successful) retry. Real wall-clock
/// (no `tokio/test-util`), kept short to stay fast.
#[tokio::test]
async fn retry_after_is_honored_on_rate_limit() {
    let provider = rate_limited_once(Duration::from_millis(600));
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("ratelimited")
        .with_retries(2)
        .with_context(vec![Message::user("hi")]);

    let start = std::time::Instant::now();
    let out = agent.run().await.expect("outcome");
    let elapsed = start.elapsed();

    assert_eq!(out.text, "recovered");
    assert!(
        matches!(out.finish, Finish::EndTurn),
        "expected EndTurn after honoring Retry-After, got {:?}",
        out.finish
    );
    assert_eq!(provider.calls(), 2, "one 429 + one success");
    // Must have waited at least most of the server-requested delay (default
    // backoff alone would be only 250 ms, so >550 ms proves Retry-After won).
    assert!(
        elapsed >= Duration::from_millis(550),
        "expected to wait ~600ms for Retry-After, waited {elapsed:?}"
    );
}

/// A provider that fails more times than allowed retries should stop after
/// `max_retries + 1` attempts.
#[tokio::test]
async fn retries_exhausted_stops_after_cap() {
    let provider = flaky(10); // always fails
    let mut agent = Agent::new(())
        .provider(provider.clone())
        .model("flaky")
        .with_retries(2)
        .with_context(vec![Message::user("hi")]);

    let out = agent.run().await.expect("outcome");

    assert_stopped(&out);
    // 1 initial attempt + 2 retries = 3 total calls.
    assert_eq!(
        provider.calls(),
        3,
        "expected exactly 3 calls (1 + 2 retries)"
    );
}
