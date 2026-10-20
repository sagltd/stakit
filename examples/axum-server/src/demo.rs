//! The client calls the demo binary (`example-client`) makes and the e2e tests
//! check: one place, so what the demo shows is exactly what the tests prove.

use stakit_client::{ActionResult, BatchResults, CallOpts, Client, ServerFrame, TransportError};

use crate::{Count, Greet, SaveImage, Saved, greet, progress, save_image, version};

/// A client for the example server at `origin`, calling as an admin.
pub fn client(origin: &str) -> Client {
    Client::builder(format!("{origin}/app"))
        .header("x-admin", "true")
        .stream_url(format!("{origin}/stream"))
        .ws_url(format!("{origin}/ws"))
        .build()
}

/// Fetches many actions in ONE request (a typed batch): `greet` for alice and
/// bob, then `version`.
pub async fn batch_greetings(client: &Client) -> Result<BatchResults, TransportError> {
    client
        .batch()
        .add(
            greet,
            Greet {
                name: "alice".to_owned(),
                user_id: Some(1),
            },
        )
        .add(
            greet,
            Greet {
                name: "bob".to_owned(),
                user_id: None,
            },
        )
        .add(version, ())
        .send()
        .await
}

/// Uploads a 10-byte "image" as `file_name` (params in the multipart `q`
/// field, the bytes as a `file` part).
pub async fn upload_image(
    client: &Client,
    file_name: &str,
) -> Result<ActionResult<Saved>, TransportError> {
    client
        .fetch_with(
            save_image,
            SaveImage {
                file_name: file_name.to_owned(),
            },
            CallOpts::new().file(vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        )
        .await
}

/// Runs the `progress` stream over the websocket, answering every
/// server→client `client_call` (`showToast`) with `"shown"`. Returns every
/// frame received before the stream's `end`.
pub async fn progress_over_ws(client: &Client, n: u64) -> Result<Vec<ServerFrame>, TransportError> {
    let mut conn = client.connect(CallOpts::new()).await?;
    conn.send(progress, Count { n }).await?;
    let mut frames = Vec::new();
    while let Some(frame) = conn.recv().await {
        let frame = frame?;
        match &frame {
            ServerFrame::ClientCall { id, .. } => conn.reply(*id, "shown").await?,
            ServerFrame::End { .. } => break,
            _ => {}
        }
        frames.push(frame);
    }
    // The server may drop the socket right after `end`; closing is best-effort.
    conn.close().await.ok();
    Ok(frames)
}
