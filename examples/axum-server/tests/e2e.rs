//! End-to-end tests for the example: spins the real axum app in-process and
//! drives it with the real `stakit-client` over every transport.

#![allow(clippy::unwrap_used)]

use futures::StreamExt as _;
use tokio::net::TcpListener;

use axum_server_example::{
    Count, Greet, Greeting, SaveImage, app, count, greet, progress, save_image, version,
};
use stakit_client::{CallOpts, Client, ServerFrame};

/// Spawns the example server on a random port; returns its origin.
async fn spawn() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app()).await.unwrap();
    });
    format!("http://{addr}")
}

fn client_for(origin: &str) -> Client {
    Client::builder(format!("{origin}/app"))
        .header("x-admin", "true")
        .stream_url(format!("{origin}/stream"))
        .ws_url(format!("{origin}/ws"))
        .build()
}

#[tokio::test]
async fn http_unary_ok() {
    let client = client_for(&spawn().await);
    let res = client
        .fetch(greet, Greet { name: "sam".to_owned(), user_id: None })
        .await
        .unwrap();
    assert_eq!(res.data().unwrap().message, "Hello, sam! (admin=true)");
}

#[tokio::test]
async fn http_unary_validation_error() {
    let client = client_for(&spawn().await);
    let res = client
        .fetch(greet, Greet { name: String::new(), user_id: None })
        .await
        .unwrap();
    let error = res.error().unwrap();
    assert_eq!(error.code, 422);
    assert!(error.fields.as_ref().unwrap().contains_key("name"));
}

#[tokio::test]
async fn http_typed_batch_many_actions() {
    let client = client_for(&spawn().await);
    let results = client
        .batch()
        .add(greet, Greet { name: "alice".to_owned(), user_id: Some(1) })
        .add(greet, Greet { name: "bob".to_owned(), user_id: None })
        .add(version, ())
        .send()
        .await
        .unwrap();
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
    let client = client_for(&spawn().await);
    let res = client
        .fetch_with(
            save_image,
            SaveImage { file_name: "e2e.png".to_owned() },
            CallOpts::new().file(vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        )
        .await
        .unwrap();
    assert_eq!(res.data().unwrap().bytes, 10);
}

#[tokio::test]
async fn http_stream_count() {
    let client = client_for(&spawn().await);
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
    let client = client_for(&spawn().await);
    let mut conn = client.connect(CallOpts::new()).await.unwrap();
    conn.send(progress, Count { n: 3 }).await.unwrap();

    let mut toasts = 0;
    let mut results = 0;
    loop {
        match conn.recv().await {
            Some(Ok(ServerFrame::ClientCall { id, action, .. })) => {
                assert_eq!(action, "showToast");
                toasts += 1;
                conn.reply(id, "shown").await.unwrap();
            }
            Some(Ok(ServerFrame::Result { .. })) => results += 1,
            Some(Ok(ServerFrame::End { .. })) | None => break,
            Some(Err(error)) => panic!("ws error: {error}"),
        }
    }
    conn.close().await.ok();
    assert_eq!(toasts, 3);
    assert_eq!(results, 3);
}
