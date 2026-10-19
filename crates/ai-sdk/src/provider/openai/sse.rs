//! Bounded byte-based SSE framing. UTF-8 is decoded only for complete lines;
//! tools and End are released only after `finish_reason` followed by [DONE].
use super::{Accum, response::decode_error};
use crate::provider::sse::{ByteStream, Framer, drive};
use crate::{EventStream, ProviderError, StreamEvent, Usage};

#[derive(Default)]
struct Decoder {
    framer: Framer,
    started: bool,
    accum: Accum,
}
impl Decoder {
    /// The terminal [`StreamEvent::End`] is released only on `[DONE]`.
    fn accept(&mut self, part: &[u8]) -> Result<Vec<StreamEvent>, ProviderError> {
        let Some(frame) = self.framer.accept(part)? else {
            return Ok(Vec::new());
        };
        if frame.data == b"[DONE]" {
            return self.accum.finish();
        }
        let event = serde_json::from_slice(&frame.data)
            .map_err(|_| decode_error("invalid OpenAI SSE JSON"))?;
        let mut events = self.accum.push(&event)?;
        if !self.started {
            self.started = true;
            events.insert(
                0,
                StreamEvent::Start {
                    usage: Usage::zero(),
                },
            );
        }
        Ok(events)
    }
}

pub(super) fn decode(bytes: impl ByteStream) -> EventStream {
    let mut decoder = Decoder::default();
    drive(
        bytes,
        move |part| decoder.accept(part),
        "OpenAI stream ended before [DONE]",
    )
}

#[cfg(test)]
#[path = "sse_test.rs"]
mod tests;
