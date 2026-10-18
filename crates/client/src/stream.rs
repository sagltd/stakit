//! HTTP streaming transport (JSONL frames).

use futures::{Stream, StreamExt as _};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use stakit_router::{Endpoint, ErrorBody};

use crate::TransportError;
use crate::client::{Client, encode_query, with_payload};
use crate::options::CallOpts;
use crate::result::ActionResult;

/// Largest a single (newline-terminated) stream frame may grow to before the
/// client gives up — bounds memory against a server that never sends `\n`.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Wire shape of one stream frame (see `docs/transport.md`).
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireFrame<T> {
    Next { data: T },
    Error { error: ErrorBody },
    End,
}

/// The status reported for a stream the client could not read to its end.
const STREAM_FAILED: u16 = 502;

/// The error yielded when the body ends (or breaks, or overflows) before an
/// `end`/`error` frame: a truncated reply must not look complete.
fn interrupted() -> ErrorBody {
    stakit_router::Error::coded(
        STREAM_FAILED,
        "stream_interrupted",
        "the stream ended before it finished",
    )
    .into()
}

/// What an undecodable line means: a `next` frame whose data does not match
/// the expected type is an error (`Some`); anything else — blank, malformed,
/// or a frame type this client does not know, like `heartbeat` — is skipped.
fn undecodable(line: &[u8]) -> Option<ErrorBody> {
    let frame: serde_json::Value = serde_json::from_slice(line).ok()?;
    (frame.get("type")?.as_str()? == "next").then(|| {
        stakit_router::Error::coded(
            STREAM_FAILED,
            "bad_frame",
            "a stream frame did not match the expected type",
        )
        .into()
    })
}

/// What one JSONL line means for the stream.
enum Line<T> {
    /// A frame to yield.
    Item(ActionResult<T>),
    /// A frame to yield that ends the stream (an error).
    Last(ActionResult<T>),
    /// `end`: the stream finished cleanly.
    End,
    /// Blank, malformed, or an unknown frame type (e.g. `heartbeat`).
    Skip,
}

fn parse_line<T: DeserializeOwned>(line: &[u8]) -> Line<T> {
    if line.is_empty() {
        return Line::Skip;
    }
    match serde_json::from_slice::<WireFrame<T>>(line) {
        Ok(WireFrame::Next { data }) => Line::Item(ActionResult::Ok(data)),
        Ok(WireFrame::Error { error }) => Line::Last(ActionResult::Error(error)),
        Ok(WireFrame::End) => Line::End,
        Err(_) => {
            undecodable(line).map_or(Line::Skip, |error| Line::Last(ActionResult::Error(error)))
        }
    }
}

impl Client {
    /// Streams a streaming action against the stream url (falls back to the base
    /// url) with default options: `POST` with the params as a JSON body. Yields
    /// one [`ActionResult`] per frame; blank, malformed and unknown frames (e.g.
    /// `heartbeat` keep-alives) are skipped.
    ///
    /// # Errors
    /// Returns `Err` if the initial request fails or answers a non-2xx status.
    /// Per-frame application errors are yielded as [`ActionResult::Error`] (and
    /// terminate the stream), and so are a `next` frame whose data does not
    /// decode (`bad_frame`) and a body that breaks or ends before its
    /// `end`/`error` frame (`stream_interrupted`).
    pub async fn stream<E>(
        &self,
        endpoint: E,
        params: E::Params,
    ) -> Result<impl Stream<Item = ActionResult<E::Output>> + use<E>, TransportError>
    where
        E: Endpoint,
        E::Params: serde::Serialize,
        E::Output: DeserializeOwned,
    {
        self.stream_with(endpoint, params, CallOpts::default())
            .await
    }

    /// Streams a streaming action with per-call overrides (url, headers, method).
    ///
    /// # Errors
    /// See [`Client::stream`].
    pub async fn stream_with<E>(
        &self,
        _endpoint: E,
        params: E::Params,
        opts: CallOpts,
    ) -> Result<impl Stream<Item = ActionResult<E::Output>> + use<E>, TransportError>
    where
        E: Endpoint,
        E::Params: serde::Serialize,
        E::Output: DeserializeOwned,
    {
        let q = encode_query(E::ACTION, &params)?;
        let method = opts.method.unwrap_or_default();
        let url = opts
            .url
            .as_deref()
            .or_else(|| self.stream_url())
            .unwrap_or_else(|| self.base_url())
            .to_owned();

        let request = with_payload(self.request(method, &url, &opts.headers), method, q);
        let response = request.send().await?.error_for_status()?;
        let byte_stream = response.bytes_stream();

        // Boxed so the returned stream is `Unpin` — callers can `.next()` it in a
        // loop without pinning it first.
        Ok(Box::pin(async_stream::stream! {
            futures::pin_mut!(byte_stream);
            let mut buf: Vec<u8> = Vec::new();
            let mut body_ended = false;
            // Leaving this loop means the stream never finished: a transport
            // error, a frame over the cap (a server that never emits a newline
            // would grow memory without bound), or the body ending early.
            while !body_ended {
                match byte_stream.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    Some(Err(_)) => break,
                    None => {
                        // JSON Lines allows the last line without its newline.
                        body_ended = true;
                        buf.push(b'\n');
                    }
                }
                if buf.len() > MAX_FRAME_BYTES {
                    break;
                }
                while let Some(newline) = buf.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buf.drain(..=newline).collect();
                    match parse_line::<E::Output>(line.strip_suffix(b"\n").unwrap_or(&line)) {
                        Line::Item(item) => yield item,
                        Line::Last(item) => {
                            yield item;
                            return;
                        }
                        Line::End => return,
                        Line::Skip => {}
                    }
                }
            }
            yield ActionResult::Error(interrupted());
        }))
    }
}
