//! `.on_error`: a builder hook (`prev => new`) that rewrites every outgoing
//! error before it reaches the client. Default is identity. Used to redact
//! sensitive detail in production while surfacing rich errors in development.
//! It must fire at every error boundary: unary replies, stream error frames,
//! and websocket `call` results.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use stakit_model::Model;
use stakit_router::{Error, Frame, Router, action};

#[derive(Model, Serialize, Deserialize)]
struct In {
    n: u32,
}

// An action whose error carries sensitive server-side detail (via `?` → 500):
// the client sees a generic message, with the real text kept in `Error::detail`.
#[derive(Debug)]
struct DbError;
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("postgres://admin:hunter2@db:5432 connection refused")
    }
}
impl std::error::Error for DbError {}

#[action]
fn explode() -> Result<String, DbError> {
    Err(DbError)
}

#[action(stream)]
fn explode_stream(_params: In) -> impl futures::Stream<Item = Result<u32, Error>> {
    async_stream::stream! {
        yield Ok(0u32);
        yield Err(Error::new(500, "boom"));
    }
}

fn payload(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    Value::Object(map)
}

// --- default: identity (no hook) ---

#[test]
fn default_hook_is_identity() {
    let router = Router::builder().ctx(()).register(explode).build();
    let out = block_on(router.on_request((), payload("explode", json!(null))));
    let env = &out["explode"];
    assert_eq!(env["error"]["code"], 500);
    // Internal errors are already generic by default; the hook didn't alter it.
    assert_eq!(env["error"]["message"], "internal server error");
}

// --- production: redact everything to a fixed message ---

#[test]
fn prod_hook_redacts_message_and_code() {
    let router = Router::builder()
        .ctx(())
        .on_error(|e| Error::new(e.code, "request failed"))
        .register(explode)
        .build();
    let out = block_on(router.on_request((), payload("explode", json!(null))));
    let env = &out["explode"];
    assert_eq!(env["error"]["code"], 500);
    assert_eq!(env["error"]["message"], "request failed");
    // The leaked DB URL never reaches the wire under any message.
    assert!(!env.to_string().contains("hunter2"));
}

// --- development: surface the server-side detail in the message ---

#[test]
fn dev_hook_can_surface_detail_for_developers() {
    let router = Router::builder()
        .ctx(())
        // In dev we *want* the gory detail; the hook receives the `Error` and can
        // read `detail()` (the original text kept server-side).
        .on_error(|e| match e.detail() {
            Some(detail) => Error::new(e.code, format!("[dev] {detail}")),
            None => e,
        })
        .register(explode)
        .build();
    let out = block_on(router.on_request((), payload("explode", json!(null))));
    let env = &out["explode"];
    assert_eq!(env["error"]["code"], 500);
    assert_eq!(
        env["error"]["message"],
        "[dev] postgres://admin:hunter2@db:5432 connection refused"
    );
}

// --- the hook fires for routing (404) errors too ---

#[test]
fn hook_fires_on_not_found() {
    let router = Router::builder()
        .ctx(())
        .on_error(|_| Error::new(404, "no"))
        .register(explode)
        .build();
    let out = block_on(router.on_request((), payload("ghost", json!(null))));
    assert_eq!(out["ghost"]["error"]["message"], "no");
}

// --- the hook fires for stream error frames ---

#[test]
fn hook_fires_on_stream_error_frames() {
    let router = Router::builder()
        .ctx(())
        .on_error(|e| Error::new(e.code, "redacted stream error"))
        .register_stream(explode_stream)
        .build();
    let frames: Vec<Frame> = block_on(
        router
            .on_stream((), payload("explode_stream", json!({ "n": 1 })))
            .collect(),
    );
    match frames.iter().find(|f| matches!(f, Frame::Error { .. })) {
        Some(Frame::Error { error, .. }) => {
            assert_eq!(error.message, "redacted stream error");
        }
        _ => panic!("expected a redacted error frame"),
    }
}

// --- the hook fires on the websocket session path ---

#[tokio::test]
async fn hook_fires_on_session_path() {
    let router = Arc::new(
        Router::<(), ()>::builder()
            .ctx(())
            .on_error(|e| Error::new(e.code, "ws redacted"))
            .register(explode)
            .build(),
    );
    let mut session = router.session(());
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    session.handle(&json!({ "kind": "call", "id": 1, "action": "explode", "params": null }));
    let result = tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], 500);
    assert_eq!(result["error"]["message"], "ws redacted");
}
