//! Proves the client's `wss://` (TLS) path end to end against a local
//! self-signed TLS websocket server — no network required. Exercises
//! `danger_accept_invalid_certs` (the internal-CA / self-signed knob).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use stakit_client::{CallOpts, Client, ServerFrame};
use stakit_model::Model;
use stakit_router::{Error, action};

#[derive(Model, Serialize, Deserialize)]
struct Greet {
    name: String,
}

#[derive(Model, Serialize, Deserialize)]
struct Greeting {
    message: String,
}

// Defines the `greet` endpoint (the client infers params/output from it).
#[action]
async fn greet(_params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: String::new(),
    })
}

/// Spawns a self-signed TLS websocket server that answers a `greet` call with a
/// result frame; returns its port.
async fn spawn_wss() -> u16 {
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
                while let Some(Ok(message)) = ws.next().await {
                    if let Message::Text(text) = message {
                        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        if frame["kind"] == "call" {
                            let id = frame["id"].as_u64().unwrap_or(0);
                            let name = frame["params"]["name"].as_str().unwrap_or("").to_owned();
                            let reply = json!({
                                "kind": "result", "id": id, "status": "ok",
                                "data": { "message": format!("hello {name} over wss") }
                            });
                            let _ = ws.send(Message::Text(reply.to_string().into())).await;
                        }
                    }
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn wss_roundtrip_with_self_signed_cert() {
    let port = spawn_wss().await;
    let client = Client::builder("https://unused.invalid")
        .danger_accept_invalid_certs(true)
        .ws_url(format!("wss://127.0.0.1:{port}"))
        .build();

    let mut conn = client.connect(CallOpts::new()).await.expect("wss connect");
    conn.send(
        greet,
        Greet {
            name: "tls".to_owned(),
        },
    )
    .await
    .unwrap();
    let frame = conn.recv().await.unwrap().unwrap();
    let ServerFrame::Result { result, .. } = frame else {
        panic!("expected a result frame, got {frame:?}");
    };
    let greeting: Greeting = serde_json::from_value(result.ok().unwrap()).unwrap();
    assert_eq!(greeting.message, "hello tls over wss");
    conn.close().await.ok();
}
