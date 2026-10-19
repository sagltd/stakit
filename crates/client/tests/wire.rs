//! Wire-format tests: what the client actually puts on the wire, checked by a
//! recording server (see `docs/transport.md`). Params ride in a POST JSON body
//! (or the multipart `q` field) and never in the URL; only an explicit GET uses
//! `?q=`. A non-2xx status is a transport error, never an envelope.

#![allow(clippy::unwrap_used)]
#![allow(clippy::missing_panics_doc)]

use std::sync::{Arc, Mutex};

use axum::Router as AxumRouter;
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};

use stakit_client::{CallOpts, Client, Method, TransportError};

#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{Count, Greet, count, greet, serve};

/// One request exactly as the server received it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    /// The decoded `?q=` value, if the url carried one.
    url_query: Option<String>,
    /// Every `content-type` header value (a duplicate would show up here).
    content_types: Vec<String>,
    body: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

#[derive(Deserialize)]
struct Q {
    q: Option<String>,
}

async fn record(log: &Log, request: Request) {
    let method = request.method().to_string();
    let url_query = Query::<Q>::try_from_uri(request.uri())
        .ok()
        .and_then(|query| query.0.q);
    let content_types = request
        .headers()
        .get_all(CONTENT_TYPE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    log.lock().unwrap().push(Seen {
        method,
        url_query,
        content_types,
        body,
    });
}

async fn rpc(State(log): State<Log>, request: Request) -> Response {
    record(&log, request).await;
    axum::Json(json!({ "greet": { "status": "ok", "data": { "message": "ok" } } })).into_response()
}

async fn stream(State(log): State<Log>, request: Request) -> Response {
    record(&log, request).await;
    "{\"type\":\"next\",\"data\":7}\n{\"type\":\"end\"}\n".into_response()
}

async fn limited() -> Response {
    (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response()
}

/// Spawns the recording server; returns its origin and the request log.
async fn spawn() -> (String, Log) {
    let log = Log::default();
    let app = AxumRouter::new()
        .route("/rpc", any(rpc))
        .route("/stream", any(stream))
        .route("/limited", any(limited))
        .with_state(Arc::clone(&log));
    (serve(app).await, log)
}

fn last(log: &Log) -> Seen {
    log.lock().unwrap().last().cloned().unwrap()
}

/// The last request was a `POST` with exactly `expected` as its JSON body and
/// nothing in the url.
fn assert_json_post(log: &Log, expected: &Value) {
    let seen = last(log);
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.url_query, None, "params must never ride in a POST url");
    assert_eq!(seen.content_types, vec!["application/json".to_owned()]);
    let body: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(&body, expected);
}

fn greet_params() -> Greet {
    Greet {
        name: "a".to_owned(),
    }
}

#[tokio::test]
async fn unary_posts_a_json_body_and_never_puts_params_in_the_url() {
    let (origin, log) = spawn().await;
    let client = Client::new(format!("{origin}/rpc"));

    let result = client.fetch(greet, greet_params()).await.unwrap();
    assert_eq!(result.data().unwrap().message, "ok");

    assert_json_post(&log, &json!({ "greet": { "name": "a" } }));
}

#[tokio::test]
async fn explicit_get_puts_params_in_the_query_and_sends_no_body() {
    let (origin, log) = spawn().await;
    let client = Client::new(format!("{origin}/rpc"));

    client
        .fetch_with(greet, greet_params(), CallOpts::new().method(Method::Get))
        .await
        .unwrap();

    let seen = last(&log);
    assert_eq!(seen.method, "GET");
    let query: Value = serde_json::from_str(&seen.url_query.unwrap()).unwrap();
    assert_eq!(query, json!({ "greet": { "name": "a" } }));
    assert!(seen.body.is_empty());
}

#[tokio::test]
async fn multipart_sends_params_as_the_q_field_and_never_in_the_url() {
    let (origin, log) = spawn().await;
    let client = Client::new(format!("{origin}/rpc"));

    client
        .fetch_with(greet, greet_params(), CallOpts::new().file(vec![1u8, 2, 3]))
        .await
        .unwrap();

    let seen = last(&log);
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.url_query, None, "params must never ride in a POST url");
    assert_eq!(seen.content_types.len(), 1);
    assert!(seen.content_types[0].starts_with("multipart/form-data"));
    let body = String::from_utf8_lossy(&seen.body);
    assert!(body.contains("name=\"q\""), "missing q field: {body}");
    assert!(
        body.contains(r#"{"greet":{"name":"a"}}"#),
        "missing payload: {body}"
    );
    assert!(body.contains("name=\"file\""), "missing file part: {body}");
}

#[tokio::test]
async fn files_with_get_are_rejected_before_anything_is_sent() {
    let (origin, log) = spawn().await;
    let client = Client::new(format!("{origin}/rpc"));

    let result = client
        .fetch_with(
            greet,
            greet_params(),
            CallOpts::new().method(Method::Get).file(vec![1u8]),
        )
        .await;

    assert!(
        matches!(result, Err(TransportError::InvalidRequest(_))),
        "expected InvalidRequest, got {result:?}"
    );
    assert!(log.lock().unwrap().is_empty(), "nothing may reach the wire");
}

#[tokio::test]
async fn a_base_content_type_header_cannot_corrupt_the_body_encoding() {
    let (origin, log) = spawn().await;
    let client = Client::builder(format!("{origin}/rpc"))
        .header("Content-Type", "text/plain")
        .build();

    client.fetch(greet, greet_params()).await.unwrap();

    assert_eq!(
        last(&log).content_types,
        vec!["application/json".to_owned()]
    );
}

#[tokio::test]
async fn stream_posts_a_json_body_and_never_puts_params_in_the_url() {
    let (origin, log) = spawn().await;
    let client = Client::builder(format!("{origin}/rpc"))
        .stream_url(format!("{origin}/stream"))
        .build();

    let items: Vec<u32> = client
        .stream(count, Count { n: 1 })
        .await
        .unwrap()
        .filter_map(|item| async move { item.ok() })
        .collect()
        .await;
    assert_eq!(items, vec![7]);

    assert_json_post(&log, &json!({ "count": { "n": 1 } }));
}

#[tokio::test]
async fn unary_non_2xx_status_is_a_transport_error() {
    let (origin, _log) = spawn().await;
    let client = Client::new(format!("{origin}/limited"));

    let result = client.fetch(greet, greet_params()).await;

    let Err(TransportError::Http(error)) = result else {
        panic!("expected an Http transport error, got {result:?}");
    };
    assert_eq!(error.status().map(|status| status.as_u16()), Some(429));
}

#[tokio::test]
async fn stream_non_2xx_status_is_a_transport_error() {
    let (origin, _log) = spawn().await;
    let client = Client::builder(format!("{origin}/rpc"))
        .stream_url(format!("{origin}/limited"))
        .build();

    let result = client.stream(count, Count { n: 1 }).await;

    let Err(TransportError::Http(error)) = result else {
        panic!("expected an Http transport error");
    };
    assert_eq!(error.status().map(|status| status.as_u16()), Some(429));
}
