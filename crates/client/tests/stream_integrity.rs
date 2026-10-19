//! A stream that ends without its `end`/`error` frame, or carries a `next`
//! frame the client cannot decode, must not look like a clean finish: the
//! consumer would treat a truncated reply as complete.

#![allow(clippy::unwrap_used)]

use axum::Router as AxumRouter;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use futures::StreamExt as _;

use stakit_client::{ActionResult, Client};

#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{Count, count, serve as serve_app};

/// Serve `lines` (newline-joined, newline-terminated) as the `/stream` body.
async fn serve(lines: &'static [&'static str]) -> Client {
    serve_body(format!("{}\n", lines.join("\n"))).await
}

/// Serve `body` verbatim as the `/stream` body.
async fn serve_body(body: String) -> Client {
    let handler = move || async move { Response::new(Body::from(body)) };
    let app = AxumRouter::new().route("/stream", post(handler));
    Client::builder(format!("{}/stream", serve_app(app).await)).build()
}

async fn items(client: &Client) -> Vec<ActionResult<u32>> {
    client
        .stream(count, Count { n: 1 })
        .await
        .unwrap()
        .collect()
        .await
}

fn error_kind(item: &ActionResult<u32>) -> Option<&str> {
    match item {
        ActionResult::Error(error) => Some(&error.kind),
        ActionResult::Ok(_) => None,
    }
}

#[tokio::test]
async fn a_body_that_ends_without_an_end_frame_reports_an_interrupted_stream() {
    let client = serve(&[r#"{"type":"next","data":1}"#]).await;

    let items = items(&client).await;

    assert_eq!(items.len(), 2, "{items:?}");
    assert!(matches!(items.first(), Some(ActionResult::Ok(1))));
    assert_eq!(
        items.last().and_then(error_kind),
        Some("stream_interrupted")
    );
}

#[tokio::test]
async fn a_next_frame_that_cannot_be_decoded_is_an_error_not_a_skip() {
    let client = serve(&[
        r#"{"type":"next","data":"not a number"}"#,
        r#"{"type":"next","data":2}"#,
        r#"{"type":"end"}"#,
    ])
    .await;

    let items = items(&client).await;

    assert_eq!(
        items.len(),
        1,
        "the stream stops at the bad frame: {items:?}"
    );
    assert_eq!(items.first().and_then(error_kind), Some("bad_frame"));
}

#[tokio::test]
async fn heartbeats_are_skipped_and_an_end_frame_finishes_cleanly() {
    let client = serve(&[
        r#"{"type":"heartbeat"}"#,
        r#"{"type":"next","data":3}"#,
        r#"{"type":"end"}"#,
    ])
    .await;

    let items = items(&client).await;

    assert_eq!(items.len(), 1, "{items:?}");
    assert!(matches!(items.first(), Some(ActionResult::Ok(3))));
}

#[tokio::test]
async fn a_last_line_without_its_newline_still_counts() {
    let client = serve_body(
        r#"{"type":"next","data":4}
{"type":"end"}"#
            .to_owned(),
    )
    .await;

    let items = items(&client).await;

    assert_eq!(items.len(), 1, "{items:?}");
    assert!(matches!(items.first(), Some(ActionResult::Ok(4))));
}
