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

use std::sync::Arc;

use axum::Router as AxumRouter;
use axum::body::Body;
use axum::extract::Query;
use axum::response::Response;
use axum::routing::{get, post};
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use stakit_client::{CallOpts, Client, TransportError};
use stakit_model::Model;
use stakit_router::{Error, action};

// ── models / endpoints (the client infers params/output from these) ──────────

#[derive(Model, Serialize, Deserialize, Debug)]
struct Greet {
    name: String,
}

#[derive(Model, Serialize, Deserialize, Debug)]
struct Greeting {
    message: String,
}

#[derive(Model, Serialize, Deserialize, Debug)]
struct Count {
    n: u32,
}

#[action]
async fn greet(_params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: String::new(),
    })
}

#[action(stream)]
fn count(_params: Count) -> impl futures::Stream<Item = Result<u32, Error>> + use<> {
    async_stream::stream! { yield Ok(0u32); }
}

#[derive(Deserialize)]
struct Q {
    q: String,
}

// ════════════════════════════════════════════════════════════════════════════
//  TLS: NoCertVerification is gated behind danger_accept_invalid_certs
// ════════════════════════════════════════════════════════════════════════════

/// Spawns a self-signed TLS websocket echo-ish server; returns its port. A real
/// (verifying) client MUST reject this cert; only the danger opt-in accepts it.
async fn spawn_self_signed_wss() -> u16 {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cert =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
            .unwrap();
    let cert_der = cert.cert.der().clone();
    let key_der =
        rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_text() {
                        let _ = ws.send(Message::Text("{}".to_string().into())).await;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn self_signed_wss_is_rejected_by_default() {
    // No danger opt-in → the default webpki-roots verifier is used → a
    // self-signed cert fails the handshake. This proves NoCertVerification is
    // NOT in the default path.
    let port = spawn_self_signed_wss().await;
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
    let port = spawn_self_signed_wss().await;
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
/// garbage, frames of an unknown `type`, a frame whose `data` is the wrong type
/// for the typed stream, then a couple of valid frames and an `end`.
async fn hostile_stream() -> Response {
    let body = [
        "",                                               // blank line, skipped
        "not json at all",                                // unparseable, skipped
        "{}",                                             // no type tag, skipped
        r#"{"type":"unknownkind"}"#,                      // unknown variant, skipped
        r#"{"type":"next","data":"a string not a u32"}"#, // type mismatch, skipped
        r#"{"type":"next","data":1}"#,                    // valid item
        r#"{"type":"next","data":2}"#,                    // valid item
        r#"{"type":"end"}"#,                              // terminator
        r#"{"type":"next","data":99}"#,                   // after end → never seen
    ]
    .join("\n");
    let mut body = body.into_bytes();
    body.push(b'\n');
    Response::new(Body::from(body))
}

async fn spawn_hostile_stream_server() -> String {
    let app = AxumRouter::new().route("/stream", post(hostile_stream).get(hostile_stream));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
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
    // malformed/mistyped/post-`end` frames are dropped — no panic, no crash.
    let values: Vec<u32> = items
        .into_iter()
        .filter_map(stakit_client::ActionResult::ok)
        .collect();
    assert_eq!(values, vec![1, 2]);
}

// ════════════════════════════════════════════════════════════════════════════
//  Unary HTTP: an unexpected response shape is a typed error, not a panic
// ════════════════════════════════════════════════════════════════════════════

async fn weird_response(Query(_q): Query<Q>) -> Response {
    // Server answers with a bare array where the unary client expects an object
    // keyed by the action name.
    Response::new(Body::from(r#"["unexpected","array"]"#))
}

async fn spawn_weird_server() -> String {
    let app = AxumRouter::new().route("/rpc", get(weird_response).post(weird_response));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
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
