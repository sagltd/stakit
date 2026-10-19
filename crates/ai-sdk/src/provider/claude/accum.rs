//! Streaming response assembly. Retain complete block boundaries and signatures
//! exactly; UI deltas remain a separate, backward-compatible event channel.
#[cfg(test)]
mod finality_test;
#[cfg(test)]
mod input_usage_test;

use super::{block::Block, map_stop_reason, map_usage, sse::decode_error};
use crate::provider::{
    MAX_STREAM_BLOCK_BYTES, MAX_STREAM_OPEN_BLOCKS, MAX_STREAM_RESPONSE_BLOCKS,
    MAX_STREAM_RESPONSE_BYTES, assistant_content_bytes,
};
use crate::{ProviderError, StopReason, StreamEvent, Usage};
use hashbrown::{HashMap, HashSet};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Accum {
    usage: Usage,
    has_input_usage: bool,
    has_final_output_usage: bool,
    // Keep unknown wire blocks open too, even though their contents are ignored.
    blocks: HashMap<u64, Option<Block>>,
    seen: HashSet<u64>,
    response_bytes: usize,
    pub(super) stop: Option<StopReason>,
}

impl Accum {
    pub(super) fn has_open_blocks(&self) -> bool {
        !self.blocks.is_empty()
    }

    fn start(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| decode_error("Claude block index missing"))?;
        if self.seen.contains(&index)
            || self.seen.len() >= MAX_STREAM_RESPONSE_BLOCKS
            || self.blocks.len() >= MAX_STREAM_OPEN_BLOCKS
        {
            return Err(decode_error(
                "duplicate or excessive Claude response blocks",
            ));
        }
        let block = event
            .get("content_block")
            .ok_or_else(|| decode_error("Claude content_block missing"))?;
        let block = Block::start(block)?;
        self.seen.insert(index);
        let size = block.as_ref().map_or(0, Block::bytes);
        if size > MAX_STREAM_BLOCK_BYTES
            || self.response_bytes.saturating_add(size) > MAX_STREAM_RESPONSE_BYTES
        {
            return Err(decode_error(
                "Claude response content exceeded its byte limit",
            ));
        }
        self.response_bytes += size;
        let events = block.as_ref().map_or_else(Vec::new, Block::initial_events);
        self.blocks.insert(index, block);
        Ok(events)
    }

    fn delta(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let Some(index) = event.get("index").and_then(Value::as_u64) else {
            return Err(decode_error("Claude block delta index missing"));
        };
        // Unknown future block types are ignored, as are their deltas. The
        // transport validates that every delta refers to an open wire block.
        let Some(block) = self.blocks.get_mut(&index).and_then(Option::as_mut) else {
            return Ok(Vec::new());
        };
        let delta = event
            .get("delta")
            .ok_or_else(|| decode_error("Claude block delta missing"))?;
        block.delta(delta, &mut self.response_bytes)
    }

    fn finish(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| decode_error("Claude block stop index missing"))?;
        let Some(Some(block)) = self.blocks.remove(&index) else {
            return Ok(Vec::new());
        };
        let before = block.bytes();
        let (content, call) = block.finish()?;
        let after = assistant_content_bytes(&content);
        let response = self
            .response_bytes
            .saturating_sub(before)
            .saturating_add(after);
        if after > MAX_STREAM_BLOCK_BYTES || response > MAX_STREAM_RESPONSE_BYTES {
            return Err(decode_error(
                "Claude response content exceeded its byte limit",
            ));
        }
        self.response_bytes = response;
        let mut events = vec![StreamEvent::AssistantBlock { index, content }];
        if let Some(call) = call {
            events.push(call);
        }
        Ok(events)
    }

    fn message_delta(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.has_open_blocks() {
            return Err(decode_error("Claude message_delta before blocks completed"));
        }
        let usage = event
            .get("usage")
            .ok_or_else(|| decode_error("Claude message_delta requires output usage"))?;
        super::usage::validate(usage)?;
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| decode_error("Claude message_delta requires output usage"))?;
        if output_tokens < self.usage.output_tokens {
            return Err(decode_error("Claude cumulative output usage decreased"));
        }
        let delta = map_usage(usage);
        let mut next_usage = self.usage;
        next_usage.output_tokens = output_tokens;
        if usage.get("input_tokens").is_some() {
            next_usage.input_tokens = delta.input_tokens;
        }
        if usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .is_some()
        {
            next_usage.cache_create_tokens = delta.cache_create_tokens;
        }
        if usage
            .get("cache_creation")
            .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
            .is_some()
        {
            next_usage.cache_create_1h_tokens = delta.cache_create_1h_tokens;
        }
        if usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .is_some()
        {
            next_usage.cache_read_tokens = delta.cache_read_tokens;
        }
        if usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .is_some()
        {
            next_usage.reasoning_tokens = delta.reasoning_tokens;
        }
        super::usage::validate_totals(&next_usage)?;
        if next_usage.total_input() < self.usage.total_input() {
            return Err(decode_error("Claude cumulative input usage decreased"));
        }
        self.usage = next_usage;
        self.has_final_output_usage = true;
        if let Some(delta) = event.get("delta") {
            if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                self.stop = Some(map_stop_reason(
                    reason,
                    delta.get("stop_sequence").and_then(Value::as_str),
                ));
            }
        }
        Ok(vec![StreamEvent::Usage { usage: self.usage }])
    }

    pub(super) fn push(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let kind = event.get("type").and_then(Value::as_str);
        if self.stop.is_some()
            && matches!(
                kind,
                Some("content_block_start" | "content_block_delta" | "content_block_stop")
            )
        {
            return Err(decode_error("Claude content followed terminal usage"));
        }
        match kind {
            Some("message_start") => {
                if let Some(usage) = event
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    super::usage::validate(usage)?;
                    self.has_input_usage =
                        usage.get("input_tokens").and_then(Value::as_u64).is_some();
                    self.usage = map_usage(usage);
                    super::usage::validate_totals(&self.usage)?;
                }
                Ok(vec![StreamEvent::Start { usage: self.usage }])
            }
            Some("content_block_start") => self.start(event),
            Some("content_block_delta") => self.delta(event),
            Some("content_block_stop") => self.finish(event),
            Some("message_delta") => self.message_delta(event),
            Some("message_stop") => {
                if !self.has_input_usage || !self.has_final_output_usage {
                    return Err(decode_error(
                        "Claude stream requires initial input and final output usage",
                    ));
                }
                if self.has_open_blocks() {
                    return Err(decode_error("Claude message_stop with incomplete blocks"));
                }
                let stop = self
                    .stop
                    .clone()
                    .ok_or_else(|| decode_error("Claude message_stop without a stop reason"))?;
                Ok(vec![StreamEvent::End {
                    stop,
                    usage: self.usage,
                }])
            }
            _ => Ok(Vec::new()),
        }
    }
}
