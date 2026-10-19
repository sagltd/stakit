//! Proves the client's `wss://` (TLS) path end to end against a local
//! self-signed TLS websocket server — no network required. Exercises
//! `danger_accept_invalid_certs` (the internal-CA / self-signed knob).

#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};

use stakit_client::Client;

#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{Greet, greet, spawn_self_signed_wss, ws_call};

/// Answers a `greet` call frame with a result frame.
fn answer_greet(text: &str) -> Option<String> {
    let frame: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    if frame["kind"] != "call" {
        return None;
    }
    let id = frame["id"].as_u64().unwrap_or(0);
    let name = frame["params"]["name"].as_str().unwrap_or("");
    let reply = json!({
        "kind": "result", "id": id, "status": "ok",
        "data": { "message": format!("hello {name} over wss") }
    });
    Some(reply.to_string())
}

#[tokio::test]
async fn wss_roundtrip_with_self_signed_cert() {
    let port = spawn_self_signed_wss(answer_greet).await;
    let client = Client::builder("https://unused.invalid")
        .danger_accept_invalid_certs(true)
        .ws_url(format!("wss://127.0.0.1:{port}"))
        .build();

    let params = Greet {
        name: "tls".to_owned(),
    };
    let (conn, greeting) = ws_call(&client, greet, params).await;
    assert_eq!(greeting.message, "hello tls over wss");
    conn.close().await.ok();
}
