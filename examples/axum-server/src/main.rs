//! Runs the example server on `127.0.0.1:3007` and writes `types.d.ts`.
//!
//! Then try the demo clients (server must be running):
//!   cargo run --bin example-client          # Rust client
//!   bun run example-client.ts                # TypeScript client

use axum_server_example::{app, build_router};

#[tokio::main]
async fn main() {
    std::fs::write("types.d.ts", build_router().generate_ts()).expect("write types.d.ts");
    println!("wrote types.d.ts");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3007")
        .await
        .unwrap();
    println!("listening on http://127.0.0.1:3007");
    axum::serve(listener, app()).await.unwrap();
}
