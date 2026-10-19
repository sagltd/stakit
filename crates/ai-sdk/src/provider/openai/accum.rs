//! Accumulates one choice; tools remain buffered until an explicit stream end.
use super::{MAX_INFLIGHT_TOOLS, api_error, map_finish_reason, map_usage, response::decode_error};
use crate::provider::{MAX_STREAM_BLOCK_BYTES, MAX_STREAM_RESPONSE_BYTES};
use crate::{ProviderError, StopReason, StreamEvent, Usage};
use serde_json::Value;

#[derive(Default)]
pub(super) struct Accum {
    usage: Usage,
    raw_usage: Value,
    has_final_output_usage: bool,
    tool_bytes: usize,
    tools: std::collections::BTreeMap<u64, ToolBuf>,
    stop: Option<StopReason>,
    refused: bool,
}
#[derive(Default)]
struct ToolBuf {
    id: String,
    name: String,
    args: String,
}

impl Accum {
    pub(super) fn push(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        if event.get("error").is_some() {
            return Err(api_error(500, &event.to_string(), None));
        }
        let choices = event["choices"]
            .as_array()
            .ok_or_else(|| decode_error("missing OpenAI stream choices"))?;
        if choices.len() > 1 {
            return Err(decode_error(
                "multiple OpenAI stream choices are unsupported",
            ));
        }
        let mut out = Vec::new();
        if let Some(usage) = event.get("usage").filter(|u| !u.is_null()) {
            if !usage.is_object() {
                return Err(decode_error("invalid OpenAI stream usage"));
            }
            super::usage::merge_usage(&mut self.raw_usage, usage)?;
            self.usage = map_usage(&self.raw_usage);
            if self.stop.is_some() {
                self.has_final_output_usage |= usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .is_some();
            }
            out.push(StreamEvent::Usage { usage: self.usage });
        }
        let Some(choice) = choices.first() else {
            if event.get("usage").is_none_or(Value::is_null) {
                return Err(decode_error("empty OpenAI stream choice without usage"));
            }
            return Ok(out);
        };
        if self.stop.is_some() {
            return Err(decode_error("OpenAI choice after finish_reason"));
        }
        if choice
            .get("index")
            .is_some_and(|index| index.as_u64() != Some(0))
        {
            return Err(decode_error("invalid OpenAI choice index"));
        }
        let delta = choice["delta"]
            .as_object()
            .ok_or_else(|| decode_error("invalid OpenAI stream delta"))?;
        for field in ["content", "refusal"] {
            if let Some(value) = delta.get(field).filter(|value| !value.is_null()) {
                let text = value
                    .as_str()
                    .ok_or_else(|| decode_error("invalid OpenAI stream text"))?;
                if !text.is_empty() {
                    self.refused |= field == "refusal";
                    out.push(StreamEvent::TextDelta(text.into()));
                }
            }
        }
        if let Some(calls) = delta.get("tool_calls").filter(|value| !value.is_null()) {
            for call in calls
                .as_array()
                .ok_or_else(|| decode_error("invalid OpenAI tool delta"))?
            {
                self.tool(call)?;
            }
        }
        if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
            let reason = reason
                .as_str()
                .filter(|reason| !reason.is_empty())
                .ok_or_else(|| decode_error("invalid OpenAI finish_reason"))?;
            self.stop = Some(map_finish_reason(reason));
            self.has_final_output_usage |= event
                .get("usage")
                .and_then(|usage| usage.get("completion_tokens"))
                .and_then(Value::as_u64)
                .is_some();
        }
        Ok(out)
    }

    fn tool(&mut self, call: &Value) -> Result<(), ProviderError> {
        let index = call["index"]
            .as_u64()
            .ok_or_else(|| decode_error("missing OpenAI tool index"))?;
        if !self.tools.contains_key(&index) && self.tools.len() >= MAX_INFLIGHT_TOOLS {
            return Err(decode_error("too many OpenAI tool calls"));
        }
        let buf = self.tools.entry(index).or_default();
        let id = call
            .get("id")
            .map(super::response::required_text)
            .transpose()?;
        if id.is_some_and(|id| !buf.id.is_empty() && buf.id != id) {
            return Err(decode_error("conflicting OpenAI tool ID"));
        }
        if call
            .get("type")
            .is_some_and(|kind| kind.as_str() != Some("function"))
        {
            return Err(decode_error("unsupported OpenAI tool kind"));
        }
        let function = call["function"]
            .as_object()
            .ok_or_else(|| decode_error("missing OpenAI tool function"))?;
        let fragment = |field: &str| -> Result<&str, ProviderError> {
            function.get(field).map_or(Ok(""), |value| {
                value
                    .as_str()
                    .ok_or_else(|| decode_error("invalid OpenAI tool fragment"))
            })
        };
        let name = fragment("name")?;
        let arguments = fragment("arguments")?;
        let previous_bytes = buf
            .id
            .len()
            .saturating_add(buf.name.len())
            .saturating_add(buf.args.len());
        let next_bytes = id
            .map_or(buf.id.len(), str::len)
            .saturating_add(buf.name.len())
            .saturating_add(name.len())
            .saturating_add(buf.args.len())
            .saturating_add(arguments.len());
        let response_bytes = self
            .tool_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(next_bytes);
        if next_bytes > MAX_STREAM_BLOCK_BYTES || response_bytes > MAX_STREAM_RESPONSE_BYTES {
            return Err(decode_error(
                "OpenAI retained tool content exceeded its byte limit",
            ));
        }
        self.tool_bytes = response_bytes;
        if let Some(id) = id {
            id.clone_into(&mut buf.id);
        }
        buf.name.push_str(name);
        buf.args.push_str(arguments);
        Ok(())
    }

    pub(super) fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        super::usage::require_complete(&self.raw_usage)?;
        if !self.has_final_output_usage {
            return Err(decode_error(
                "OpenAI stream requires final completion usage",
            ));
        }
        let reason = self
            .stop
            .take()
            .ok_or_else(|| decode_error("OpenAI DONE before finish_reason"))?;
        let stop = if self.refused {
            StopReason::Refusal
        } else {
            reason
        };
        let mut out = Vec::new();
        // A length/refusal stop never exposes partial tools for execution.
        if stop == StopReason::ToolUse {
            if self.tools.is_empty() {
                return Err(decode_error("OpenAI tool termination without calls"));
            }
            let mut ids = std::collections::HashSet::new();
            for (expected, (index, buf)) in (0_u64..).zip(&self.tools) {
                if expected != *index {
                    return Err(decode_error("OpenAI tool indices are incomplete"));
                }
                if buf.id.is_empty() || buf.name.is_empty() || !ids.insert(&buf.id) {
                    return Err(decode_error("missing or duplicate OpenAI tool identity"));
                }
            }
            for (_, buf) in std::mem::take(&mut self.tools) {
                out.push(StreamEvent::ToolCall {
                    id: buf.id,
                    name: buf.name,
                    // Preserve even empty/whitespace arguments: the agent must
                    // report malformed JSON instead of dispatching a fabricated object.
                    arguments: buf.args,
                });
            }
        } else if stop == StopReason::EndTurn && !self.tools.is_empty() {
            return Err(decode_error("OpenAI tool calls without tool termination"));
        }
        out.push(StreamEvent::End {
            stop,
            usage: self.usage,
        });
        Ok(out)
    }
}

#[cfg(test)]
#[path = "accum_test.rs"]
mod tests;
