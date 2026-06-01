//! Live demo of the **Rust** client against the running example server.
//!
//! Start the server first (`cargo run --bin axum-server-example`), then:
//!   cargo run --bin example-client

use axum_server_example::{Count, Greet, SaveImage, count, greet, progress, save_image};
use futures::StreamExt as _;
use stakit_client::{CallOpts, Client, ServerFrame};

#[tokio::main]
async fn main() {
    let base = "http://127.0.0.1:3007";
    let client = Client::builder(format!("{base}/app"))
        .header("x-admin", "true")
        .stream_url(format!("{base}/stream"))
        .ws_url(format!("{base}/ws"))
        .build();

    println!("== HTTP unary: greet ==");
    let r = client
        .fetch(greet, Greet { name: "sam".to_owned(), user_id: Some(7) })
        .await
        .unwrap();
    println!("  greet -> {:?}", r.data().map(|g| g.message.as_str()));

    println!("== HTTP unary: validation error ==");
    let r = client
        .fetch(greet, Greet { name: String::new(), user_id: None })
        .await
        .unwrap();
    println!("  greet(\"\") -> error {:?}", r.error());

    println!("== HTTP: MANY actions in ONE request (heterogeneous) ==");
    let out = client
        .fetch_raw(
            serde_json::json!({ "greet": { "name": "alice", "userId": 1 }, "version": null }),
            CallOpts::new(),
        )
        .await
        .unwrap();
    println!("  greet   -> {}", out["greet"]["data"]["message"]);
    println!("  version -> {}", out["version"]["data"]);

    println!("== HTTP: MANY (ordered array, duplicates allowed) ==");
    let out = client
        .fetch_raw(
            serde_json::json!([
                ["greet", { "name": "a" }],
                ["greet", { "name": "b" }],
                ["version", null]
            ]),
            CallOpts::new(),
        )
        .await
        .unwrap();
    println!("  [0] -> {}", out[0]["data"]["message"]);
    println!("  [1] -> {}", out[1]["data"]["message"]);
    println!("  [2] -> {}", out[2]["data"]);

    println!("== HTTP files: save_image (multipart) ==");
    let r = client
        .fetch_with(
            save_image,
            SaveImage { file_name: "demo.png".to_owned() },
            CallOpts::new().file(vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        )
        .await
        .unwrap();
    println!(
        "  save_image -> {:?}",
        r.data().map(|s| (s.bytes, s.path.as_str()))
    );

    println!("== HTTP stream: count ==");
    let mut stream = client.stream(count, Count { n: 4 }).await.unwrap();
    while let Some(item) = stream.next().await {
        println!("  count item -> {:?}", item.ok());
    }

    println!("== WS: progress + server->client client_call(showToast) ==");
    let mut conn = client.connect(CallOpts::new()).await.unwrap();
    conn.send(progress, Count { n: 3 }).await.unwrap();
    loop {
        match conn.recv().await {
            Some(Ok(ServerFrame::ClientCall { id, action, data })) => {
                println!("  server called client action '{action}' with {data}");
                conn.reply(id, "shown").await.unwrap();
            }
            Some(Ok(ServerFrame::Result { result, .. })) => {
                println!("  progress item -> {result:?}");
            }
            Some(Ok(ServerFrame::End { .. })) | None => break,
            Some(Err(error)) => {
                println!("  ws error: {error}");
                break;
            }
        }
    }
    conn.close().await.unwrap();
    println!("== done ==");
}
