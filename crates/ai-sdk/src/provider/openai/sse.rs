//! Bounded byte-based SSE framing. UTF-8 is decoded only for complete lines;
//! tools and End are released only after `finish_reason` followed by [DONE].
use super::{Accum, response::decode_error};
use crate::provider::sse::Framer;
use crate::{EventStream, ProviderError, StreamEvent, Usage};
use futures::{Stream, StreamExt};

#[derive(Default)]
struct Decoder {
    framer: Framer,
    started: bool,
    accum: Accum,
}
impl Decoder {
    fn accept(&mut self, part: &[u8]) -> Result<(Vec<StreamEvent>, bool), ProviderError> {
        let Some(frame) = self.framer.accept(part)? else {
            return Ok((Vec::new(), false));
        };
        if frame.data == b"[DONE]" {
            return self.accum.finish().map(|events| (events, true));
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
        Ok((events, false))
    }
}

pub(super) fn decode<S, E>(bytes: S) -> EventStream
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let mut bytes = Box::pin(bytes);
    async_stream::stream! {
        let mut decoder=Decoder::default();
        while let Some(chunk)=bytes.next().await {
            let chunk=match chunk {Ok(chunk)=>chunk,Err(error)=>{yield Err(ProviderError::Transport(error.to_string()));return;}};
            for part in chunk.split_inclusive(|byte| *byte==b'\n') {
                match decoder.accept(part) {
                    Ok((events,done))=>{ for event in events {yield Ok(event);} if done {return;} },
                    Err(error)=>{yield Err(error);return;}
                }
            }
        }
        yield Err(ProviderError::Transport("OpenAI stream ended before [DONE]".into()));
    }.boxed()
}

#[cfg(test)]
#[path = "sse_test.rs"]
mod tests;
