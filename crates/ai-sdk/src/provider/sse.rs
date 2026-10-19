//! Shared bounded SSE framing. Vendor modules validate JSON and termination.
use futures::{Stream, StreamExt};

use crate::{EventStream, ProviderError, StreamEvent};

pub(super) const MAX_FRAME_BYTES: usize = 1024 * 1024;
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub(super) event: Option<String>,
    pub(super) data: Vec<u8>,
}
#[derive(Default)]
pub(super) struct Framer {
    line: Vec<u8>,
    data: Vec<u8>,
    event: Option<String>,
}
fn decode_error(message: &str) -> ProviderError {
    ProviderError::Decode {
        err: message.into(),
        body: String::new(),
    }
}
impl Framer {
    /// Accept one line segment, optionally ending in LF. Callers split chunks
    /// with `split_inclusive` so each byte is scanned once without front drains.
    pub(super) fn accept(&mut self, part: &[u8]) -> Result<Option<Frame>, ProviderError> {
        let (bytes, complete) = part
            .strip_suffix(b"\n")
            .map_or((part, false), |line| (line, true));
        if self.line.len().saturating_add(bytes.len()) > MAX_FRAME_BYTES {
            return Err(decode_error("SSE line exceeded its byte limit"));
        }
        self.line.extend_from_slice(bytes);
        if !complete {
            return Ok(None);
        }
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| decode_error("SSE line is not valid UTF-8"))?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            let event = self.event.take();
            self.line.clear();
            if self.data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Frame {
                event,
                data: std::mem::take(&mut self.data),
            }));
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.into()),
            "data" => {
                let separator = usize::from(!self.data.is_empty());
                if self
                    .data
                    .len()
                    .saturating_add(separator)
                    .saturating_add(value.len())
                    > MAX_FRAME_BYTES
                {
                    return Err(decode_error("SSE event exceeded its byte limit"));
                }
                if separator != 0 {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value.as_bytes());
            }
            _ => {} // Comments, id, retry, and future optional fields.
        }
        self.line.clear();
        Ok(None)
    }
}

/// Drives a vendor decoder over a byte stream. `accept` takes one line
/// segment (see [`Framer::accept`]); the stream ends after the terminal
/// [`StreamEvent::End`] or the first error, and a body that closes before
/// `End` fails with `eof`.
/// A response body: a sendable stream of byte chunks or transport errors.
pub(super) trait ByteStream:
    Stream<Item = Result<bytes::Bytes, Self::Error>> + Send + 'static
{
    /// The transport error, surfaced as [`ProviderError::Transport`].
    type Error: std::fmt::Display + Send + 'static;
}

impl<S, E> ByteStream for S
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    type Error = E;
}

pub(super) fn drive<D>(bytes: impl ByteStream, mut accept: D, eof: &'static str) -> EventStream
where
    D: FnMut(&[u8]) -> Result<Vec<StreamEvent>, ProviderError> + Send + 'static,
{
    let mut bytes = Box::pin(bytes);
    async_stream::stream! {
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => { yield Err(ProviderError::Transport(error.to_string())); return; }
            };
            // No complete-chunk output queue and no front-drain: retain at most
            // one bounded line/frame plus the bounded open tool arguments.
            for part in chunk.split_inclusive(|byte| *byte == b'\n') {
                match accept(part) {
                    Ok(events) => for event in events {
                        let is_end = matches!(event, StreamEvent::End { .. });
                        yield Ok(event);
                        if is_end { return; }
                    },
                    Err(error) => { yield Err(error); return; }
                }
            }
        }
        yield Err(ProviderError::Transport(eof.to_owned()));
    }
    .boxed()
}

#[cfg(test)]
#[path = "sse_test.rs"]
mod tests;
