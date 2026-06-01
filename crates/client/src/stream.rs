//! HTTP streaming transport (JSONL frames).

use futures::{Stream, StreamExt as _};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use stakit_router::{Endpoint, ErrorBody};

use crate::TransportError;
use crate::client::{Client, encode_query};
use crate::options::CallOpts;
use crate::result::ActionResult;

/// Wire shape of one stream frame (see `docs/transport.md`).
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireFrame<T> {
    Next { data: T },
    Error { error: ErrorBody },
    End,
}

impl Client {
    /// Streams a streaming action against the stream url (falls back to the base
    /// url) with default options. Yields one [`ActionResult`] per frame.
    ///
    /// # Errors
    /// Returns `Err` if the initial request fails. Per-frame application errors
    /// are yielded as [`ActionResult::Error`] (and terminate the stream).
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
        let method: reqwest::Method = opts.method.map_or(reqwest::Method::POST, Into::into);
        let url = opts
            .url
            .as_deref()
            .or_else(|| self.stream_url())
            .unwrap_or_else(|| self.base_url())
            .to_owned();

        let mut request = self
            .http()
            .request(method, &url)
            .query(&[("q", q.as_str())]);
        for (name, value) in self.merged_headers(&opts.headers) {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let byte_stream = response.bytes_stream();

        // Boxed so the returned stream is `Unpin` — callers can `.next()` it in a
        // loop without pinning it first.
        Ok(Box::pin(async_stream::stream! {
            futures::pin_mut!(byte_stream);
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = byte_stream.next().await {
                let Ok(chunk) = chunk else { break };
                buf.extend_from_slice(&chunk);
                while let Some(newline) = buf.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buf.drain(..=newline).collect();
                    let line = &line[..line.len() - 1];
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_slice::<WireFrame<E::Output>>(line) {
                        Ok(WireFrame::Next { data }) => yield ActionResult::Ok(data),
                        Ok(WireFrame::Error { error }) => {
                            yield ActionResult::Error(error);
                            return;
                        }
                        Ok(WireFrame::End) => return,
                        Err(_) => {}
                    }
                }
            }
        }))
    }
}
