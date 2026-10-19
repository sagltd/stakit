//! End-to-end tests for the example: spins the real axum app in-process and
//! drives it with the real `stakit-client` over every transport.

#![allow(clippy::unwrap_used)]

use futures::StreamExt as _;
use tokio::net::TcpListener;

use axum_server_example::demo::{batch_greetings, client, progress_over_ws, upload_image};
use axum_server_example::{Count, Greet, Greeting, app, count, greet};
use stakit_client::ServerFrame;

/// Spawns the example server on a random port; returns its origin.
async fn spawn() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app()).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn http_unary_ok() {
    let client = client(&spawn().await);
    let res = client
        .fetch(
            greet,
            Greet {
                name: "sam".to_owned(),
                user_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.data().unwrap().message, "Hello, sam! (admin=true)");
}

#[tokio::test]
async fn http_unary_validation_error() {
    let client = client(&spawn().await);
    let res = client
        .fetch(
            greet,
            Greet {
                name: String::new(),
                user_id: None,
            },
        )
        .await
        .unwrap();
    let error = res.error().unwrap();
    assert_eq!(error.code, 422);
    assert!(error.fields.as_ref().unwrap().contains_key("name"));
}

#[tokio::test]
async fn http_typed_batch_many_actions() {
    let client = client(&spawn().await);
    let results = batch_greetings(&client).await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(
        results.get::<Greeting>(0).unwrap().data().unwrap().message,
        "Hello, alice! (admin=true)"
    );
    assert_eq!(
        results.get::<Greeting>(1).unwrap().data().unwrap().message,
        "Hello, bob! (admin=true)"
    );
    assert_eq!(
        results.get::<String>(2).unwrap().data().unwrap(),
        "stakit-example/0.1.0"
    );
}

#[tokio::test]
async fn http_multipart_file_upload() {
    let client = client(&spawn().await);
    let res = upload_image(&client, "e2e.png").await.unwrap();
    assert_eq!(res.data().unwrap().bytes, 10);
}

#[tokio::test]
async fn http_stream_count() {
    let client = client(&spawn().await);
    let mut stream = client.stream(count, Count { n: 4 }).await.unwrap();
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        if let Some(value) = item.ok() {
            items.push(value);
        }
    }
    assert_eq!(items, vec![0, 1, 2, 3]);
}

#[tokio::test]
async fn websocket_progress_with_client_call() {
    let client = client(&spawn().await);
    let frames = progress_over_ws(&client, 3).await.unwrap();
    let toasts = frames
        .iter()
        .filter(|frame| matches!(frame, ServerFrame::ClientCall { action, .. } if action == "showToast"))
        .count();
    let results = frames
        .iter()
        .filter(|frame| matches!(frame, ServerFrame::Result { .. }))
        .count();
    assert_eq!(toasts, 3);
    assert_eq!(results, 3);
}
