//! Byte-buffered, bounded SSE decoding. Each incoming byte is scanned once;
//! completed lines are cleared rather than draining or rescanning a prefix.
use super::{Accum, MAX_INFLIGHT_TOOLS};
use crate::provider::sse::{ByteStream, Framer, drive};
use crate::{EventStream, ProviderError, StreamEvent};
use hashbrown::HashMap;
use serde_json::Value;

mod frame;
#[cfg(test)]
#[path = "sse_block_bounds_test.rs"]
mod sse_block_bounds_test;
#[cfg(test)]
#[path = "sse_block_test.rs"]
mod sse_block_test;
#[cfg(test)]
#[path = "sse_bounds_test.rs"]
mod sse_bounds_test;
#[cfg(test)]
#[path = "sse_protocol_test.rs"]
mod sse_protocol_test;
#[cfg(test)]
#[path = "sse_test.rs"]
mod sse_test;
mod validation;

pub(super) fn decode_error(message: impl Into<String>) -> ProviderError {
    ProviderError::Decode {
        err: message.into(),
        body: String::new(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
    Other,
}

#[derive(Default)]
struct Decoder {
    framer: Framer,
    accum: Accum,
    blocks: HashMap<u64, BlockKind>,
    started: bool,
}

impl Decoder {
    fn event(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let kind = validation::validate(event)?;
        match kind {
            "error" => return Err(validation::stream_error(event)),
            "message_start" => {
                if self.started {
                    return Err(decode_error("duplicate Claude message_start"));
                }
                self.started = true;
            }
            "content_block_start"
            | "content_block_delta"
            | "content_block_stop"
            | "message_delta"
            | "message_stop" => {
                if !self.started {
                    return Err(decode_error("Claude event before message_start"));
                }
            }
            _ => return Ok(Vec::new()),
        }
        match kind {
            "content_block_start" => {
                let index = validation::index(event)?;
                if self.blocks.contains_key(&index) || self.blocks.len() >= MAX_INFLIGHT_TOOLS {
                    return Err(decode_error("duplicate or excessive Claude content blocks"));
                }
                let kind = match event
                    .get("content_block")
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("text") => BlockKind::Text,
                    Some("thinking") => BlockKind::Thinking,
                    Some("tool_use") => BlockKind::Tool,
                    _ => BlockKind::Other,
                };
                self.blocks.insert(index, kind);
            }
            "content_block_delta" => {
                let index = validation::index(event)?;
                let block = self
                    .blocks
                    .get(&index)
                    .ok_or_else(|| decode_error("Claude delta for an unopened block"))?;
                let delta = event
                    .get("delta")
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str);
                let expected = match delta {
                    Some("text_delta") => Some(BlockKind::Text),
                    Some("thinking_delta" | "signature_delta") => Some(BlockKind::Thinking),
                    Some("input_json_delta") => Some(BlockKind::Tool),
                    _ => None,
                };
                if expected.is_some_and(|expected| expected != *block) {
                    return Err(decode_error(
                        "Claude delta does not match its content block",
                    ));
                }
            }
            "content_block_stop" => {
                if self.blocks.remove(&validation::index(event)?).is_none() {
                    return Err(decode_error("Claude stop for an unopened block"));
                }
            }
            "message_stop"
                if !self.blocks.is_empty()
                    || self.accum.has_open_blocks()
                    || self.accum.stop.is_none() =>
            {
                return Err(decode_error(
                    "Claude message_stop before blocks and stop reason completed",
                ));
            }
            _ => {}
        }
        self.accum.push(event)
    }

    /// `part` is at most one line segment, optionally ending in LF. UTF-8 is
    /// decoded only once that complete line has arrived, including split CRLF.
    fn accept(&mut self, part: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        let Some(raw) = self.framer.accept(part)? else {
            return Ok(Vec::new());
        };
        self.event(&frame::decode(raw)?)
    }
}

pub(super) fn decode(bytes: impl ByteStream) -> EventStream {
    let mut decoder = Decoder::default();
    drive(
        bytes,
        move |part| decoder.accept(part),
        "Claude stream ended before message_stop",
    )
}
