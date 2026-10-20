//! Live demo of the **Rust** client against the running example server.
//!
//! Start the server first (`cargo run --bin axum-server-example`), then:
//!   cargo run --bin example-client

use axum_server_example::demo::{batch_greetings, client, progress_over_ws, upload_image};
use axum_server_example::{Count, Greet, Greeting, count, greet};
use futures::StreamExt as _;
use stakit_client::ServerFrame;

#[tokio::main]
async fn main() {
    let client = client("http://127.0.0.1:3007");

    println!("== HTTP unary: greet ==");
    let r = client
        .fetch(
            greet,
            Greet {
                name: "sam".to_owned(),
                user_id: Some(7),
            },
        )
        .await
        .unwrap();
    println!("  greet -> {:?}", r.data().map(|g| g.message.as_str()));

    println!("== HTTP unary: validation error ==");
    let r = client
        .fetch(
            greet,
            Greet {
                name: String::new(),
                user_id: None,
            },
        )
        .await
        .unwrap();
    println!("  greet(\"\") -> error {:?}", r.error());

    println!("== HTTP: fetch MANY actions in ONE request (typed batch) ==");
    let results = batch_greetings(&client).await.unwrap();
    println!("  {} results in one round-trip:", results.len());
    let g0 = results.get::<Greeting>(0).unwrap();
    let g1 = results.get::<Greeting>(1).unwrap();
    let ver = results.get::<String>(2).unwrap();
    println!(
        "  [0] greet   -> {:?}",
        g0.data().map(|g| g.message.as_str())
    );
    println!(
        "  [1] greet   -> {:?}",
        g1.data().map(|g| g.message.as_str())
    );
    println!("  [2] version -> {:?}", ver.data());

    println!("== HTTP files: save_image (multipart) ==");
    let r = upload_image(&client, "demo.png").await.unwrap();
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
    match progress_over_ws(&client, 3).await {
        Ok(frames) => {
            for frame in frames {
                match frame {
                    ServerFrame::ClientCall { action, data, .. } => {
                        println!("  server called client action '{action}' with {data}");
                    }
                    ServerFrame::Result { result, .. } => {
                        println!("  progress item -> {result:?}");
                    }
                    _ => {}
                }
            }
        }
        Err(error) => println!("  ws error: {error}"),
    }
    println!("== done ==");
}
