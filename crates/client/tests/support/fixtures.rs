//! Fixtures shared by the client integration tests: the `greet` / `count`
//! endpoints the client infers params and output from, an axum server spawner,
//! a self-signed TLS websocket server and a one-call websocket round trip.
//! Each test binary uses a subset.

#![allow(dead_code, unreachable_pub)]

use std::sync::Arc;

use axum::Router as AxumRouter;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use stakit_client::{CallOpts, Client, ClientEndpoint, Connection, ServerFrame};
use stakit_model::Model;
use stakit_router::{Error, action};

#[derive(Model, Serialize, Deserialize, Debug)]
pub struct Greet {
    pub name: String,
}

#[derive(Model, Serialize, Deserialize, Debug)]
pub struct Greeting {
    pub message: String,
}

#[derive(Model, Serialize, Deserialize, Debug)]
pub struct Count {
    pub n: u32,
}

/// Defines the `greet` endpoint; the test servers answer it themselves.
#[action]
pub async fn greet(_params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: String::new(),
    })
}

/// Defines the `count` stream endpoint; the test servers answer it themselves.
#[action(stream)]
pub fn count(_params: Count) -> impl futures::Stream<Item = Result<u32, Error>> + use<> {
    async_stream::stream! { yield Ok(0u32); }
}

/// Serves `app` on an ephemeral local port; returns its origin
/// (`http://127.0.0.1:PORT`).
pub async fn serve(app: AxumRouter) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Spawns a websocket server behind a self-signed TLS cert; returns its port.
/// Every text message is passed to `reply`, and what it returns is sent back.
/// A verifying client MUST reject this cert; only the danger opt-in accepts it.
pub async fn spawn_self_signed_wss(reply: fn(&str) -> Option<String>) -> u16 {
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
                    let Message::Text(text) = message else {
                        continue;
                    };
                    if let Some(answer) = reply(&text) {
                        let _ = ws.send(Message::Text(answer.into())).await;
                    }
                }
            });
        }
    });
    port
}

/// Opens a websocket connection, calls `endpoint` once and decodes its result
/// frame. Returns the connection too, so the caller decides how to close it.
pub async fn ws_call<E: ClientEndpoint>(
    client: &Client,
    endpoint: E,
    params: E::Params,
) -> (Connection, E::Output) {
    let mut conn = client.connect(CallOpts::new()).await.expect("ws connect");
    conn.send(endpoint, params).await.unwrap();
    let frame = conn.recv().await.unwrap().unwrap();
    let ServerFrame::Result { result, .. } = frame else {
        panic!("expected a result frame, got {frame:?}");
    };
    let output = serde_json::from_value(result.ok().unwrap()).unwrap();
    (conn, output)
}
