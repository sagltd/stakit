//! Security + safety audit tests for `stakit-client`.
//!
//! Treats the **wire frames from the server** (HTTP JSON body, JSONL stream
//! frames, websocket frames) as untrusted and proves the client neither panics
//! nor silently trusts something it shouldn't. Mirrors the harness style of
//! `tests/parity.rs` (axum server + the real client) and `tests/wss.rs`
//! (self-signed TLS websocket server).
//!
//! Findings / guards these lock down:
//! - the rustls `NoCertVerification` verifier is reachable ONLY behind the
//!   explicit `danger_accept_invalid_certs(true)` opt-in: a self-signed `wss://`
//!   server is *rejected* by default and *accepted* only when opted in.
//! - the JSONL stream parser drops malformed / mistyped frames instead of
//!   panicking, and a frame whose `data` is the wrong type does not crash.
//! - a non-object / array-shaped HTTP response is a typed `TransportError`,
//!   never a panic or an out-of-bounds index.

#![allow(clippy::unwrap_used)]
#![allow(clippy::missing_panics_doc)]
#![allow(dead_code)]

use axum::Router as AxumRouter;
use axum::body::Body;
use axum::response::Response;
use axum::routing::{get, post};
use futures::StreamExt as _;

use stakit_client::{CallOpts, Client, TransportError};

#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{Count, Greet, Greeting, count, greet, serve, spawn_self_signed_wss};

// ════════════════════════════════════════════════════════════════════════════
//  TLS: NoCertVerification is gated behind danger_accept_invalid_certs
// ════════════════════════════════════════════════════════════════════════════

/// A self-signed TLS websocket server answering every text frame with `{}`.
async fn spawn_echoish_wss() -> u16 {
    spawn_self_signed_wss(|_| Some("{}".to_owned())).await
}

#[tokio::test]
async fn self_signed_wss_is_rejected_by_default() {
    // No danger opt-in → the default webpki-roots verifier is used → a
    // self-signed cert fails the handshake. This proves NoCertVerification is
    // NOT in the default path.
    let port = spawn_echoish_wss().await;
    let client = Client::builder("https://unused.invalid")
        .ws_url(format!("wss://127.0.0.1:{port}"))
        .build();
    let result = client.connect(CallOpts::new()).await;
    // `Connection` has no `Debug`, so describe the outcome without printing it.
    match result {
        Err(TransportError::WebSocket(_)) => {}
        Err(other) => panic!("expected a WebSocket TLS rejection, got {other:?}"),
        Ok(_) => panic!("a self-signed cert must be rejected without the danger opt-in"),
    }
}

#[tokio::test]
async fn self_signed_wss_accepted_only_with_explicit_opt_in() {
    // Same server, but with the explicit danger opt-in → NoCertVerification is
    // installed and the handshake succeeds. Proves the verifier is reachable
    // ONLY through this knob.
    let port = spawn_echoish_wss().await;
    let client = Client::builder("https://unused.invalid")
        .danger_accept_invalid_certs(true)
        .ws_url(format!("wss://127.0.0.1:{port}"))
        .build();
    let conn = client.connect(CallOpts::new()).await;
    assert!(
        conn.is_ok(),
        "danger opt-in must accept the self-signed cert, got {:?}",
        conn.err()
    );
    conn.unwrap().close().await.ok();
}

// ════════════════════════════════════════════════════════════════════════════
//  JSONL stream parser: hostile / malformed frames must not panic
// ════════════════════════════════════════════════════════════════════════════

/// A stream endpoint that emits a hostile JSONL body: blank lines, non-JSON
/// garbage, frames of an unknown `type`, then a couple of valid frames and an
/// `end`. (A `next` frame whose `data` is the wrong type is an error, not a
/// skip: see `tests/stream_integrity.rs`.)
async fn hostile_stream() -> Response {
    let body = [
        "",                             // blank line, skipped
        "not json at all",              // unparseable, skipped
        "{}",                           // no type tag, skipped
        r#"{"type":"unknownkind"}"#,    // unknown variant, skipped
        r#"{"type":"next","data":1}"#,  // valid item
        r#"{"type":"next","data":2}"#,  // valid item
        r#"{"type":"end"}"#,            // terminator
        r#"{"type":"next","data":99}"#, // after end → never seen
    ]
    .join("\n");
    let mut body = body.into_bytes();
    body.push(b'\n');
    Response::new(Body::from(body))
}

async fn spawn_hostile_stream_server() -> String {
    serve(AxumRouter::new().route("/stream", post(hostile_stream).get(hostile_stream))).await
}

#[tokio::test]
async fn jsonl_stream_drops_malformed_frames_without_panicking() {
    let origin = spawn_hostile_stream_server().await;
    let client = Client::builder(format!("{origin}/stream"))
        .stream_url(format!("{origin}/stream"))
        .build();

    let stream = client.stream(count, Count { n: 1 }).await.unwrap();
    let items: Vec<_> = stream.collect().await;

    // Only the two well-formed, correctly-typed `next` frames survive; the
    // malformed/unknown/post-`end` frames are dropped — no panic, no crash.
    let values: Vec<u32> = items
        .into_iter()
        .filter_map(stakit_client::ActionResult::ok)
        .collect();
    assert_eq!(values, vec![1, 2]);
}

// ════════════════════════════════════════════════════════════════════════════
//  Unary HTTP: an unexpected response shape is a typed error, not a panic
// ════════════════════════════════════════════════════════════════════════════

async fn weird_response() -> Response {
    // Server answers with a bare array where the unary client expects an object
    // keyed by the action name.
    Response::new(Body::from(r#"["unexpected","array"]"#))
}

async fn spawn_weird_server() -> String {
    serve(AxumRouter::new().route("/rpc", get(weird_response).post(weird_response))).await
}

#[tokio::test]
async fn unary_unexpected_response_shape_is_typed_error_not_panic() {
    let origin = spawn_weird_server().await;
    let client = Client::new(format!("{origin}/rpc"));
    let result = client
        .fetch(
            greet,
            Greet {
                name: "x".to_owned(),
            },
        )
        .await;
    // A non-object response → MissingAction, never an index/panic.
    assert!(
        matches!(result, Err(TransportError::MissingAction(_))),
        "expected a typed MissingAction error, got {result:?}"
    );
}

#[tokio::test]
async fn batch_index_out_of_range_is_typed_error_not_panic() {
    // A non-network safety check: indexing a batch past its end is a typed
    // error, never an out-of-bounds panic.
    let origin = spawn_weird_server().await;
    let client = Client::new(format!("{origin}/rpc"));
    // The weird server returns an array, satisfying the batch's array contract;
    // indexing past the end must be IndexOutOfRange.
    let batch = client.batch().add(
        greet,
        Greet {
            name: "x".to_owned(),
        },
    );
    let results = batch.send().await.unwrap();
    let oob = results.get::<Greeting>(999);
    assert!(
        matches!(oob, Err(TransportError::IndexOutOfRange(999))),
        "expected IndexOutOfRange, got {oob:?}"
    );
}
