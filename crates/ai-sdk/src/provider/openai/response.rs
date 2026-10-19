//! Strict non-streaming response mapping; incomplete data is never invented.
use super::{map_finish_reason, map_usage};
use crate::{AssistantContent, ChatResponse, ProviderError, StopReason};
use serde_json::Value;

pub(super) fn decode_error(message: &str) -> ProviderError {
    ProviderError::Decode {
        err: message.into(),
        body: String::new(),
    }
}

pub(super) fn parse_response(value: &Value) -> Result<ChatResponse, ProviderError> {
    let choices = value["choices"]
        .as_array()
        .ok_or_else(|| decode_error("missing OpenAI choices"))?;
    if choices.len() != 1 {
        return Err(decode_error("expected exactly one OpenAI choice"));
    }
    let choice = &choices[0];
    let reason = choice["finish_reason"]
        .as_str()
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| decode_error("missing OpenAI finish_reason"))?;
    let message = choice["message"]
        .as_object()
        .ok_or_else(|| decode_error("missing OpenAI message"))?;
    let mut content = Vec::new();
    for field in ["content", "refusal"] {
        if let Some(value) = message.get(field).filter(|value| !value.is_null()) {
            let text = value
                .as_str()
                .ok_or_else(|| decode_error("invalid OpenAI message text"))?;
            if !text.is_empty() {
                content.push(AssistantContent::Text(text.into()));
            }
        }
    }
    let refused = message
        .get("refusal")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty());
    let stop = if refused {
        StopReason::Refusal
    } else {
        map_finish_reason(reason)
    };
    if let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) {
        let calls = calls
            .as_array()
            .ok_or_else(|| decode_error("invalid OpenAI tool calls"))?;
        if !calls.is_empty() && stop != StopReason::ToolUse {
            return Err(decode_error("OpenAI tool calls without tool termination"));
        }
        let mut ids = std::collections::HashSet::new();
        for call in calls {
            if call
                .get("type")
                .is_some_and(|kind| kind.as_str() != Some("function"))
            {
                return Err(decode_error("unsupported OpenAI tool kind"));
            }
            let id = required_text(&call["id"])?;
            if !ids.insert(id) {
                return Err(decode_error("duplicate OpenAI tool call ID"));
            }
            let function = &call["function"];
            let name = required_text(&function["name"])?;
            let arguments = function["arguments"]
                .as_str()
                .ok_or_else(|| decode_error("missing OpenAI tool arguments"))?;
            let input = serde_json::from_str(arguments)
                .map_err(|_| decode_error("invalid OpenAI tool arguments JSON"))?;
            content.push(AssistantContent::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            });
        }
    }
    if stop == StopReason::ToolUse
        && !content
            .iter()
            .any(|part| matches!(part, AssistantContent::ToolUse { .. }))
    {
        return Err(decode_error("OpenAI tool termination without a call"));
    }
    let mut raw_usage = Value::Null;
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        super::usage::merge_usage(&mut raw_usage, usage)?;
    }
    super::usage::require_complete(&raw_usage)?;
    Ok(ChatResponse {
        content,
        stop,
        usage: map_usage(&raw_usage),
    })
}

pub(super) fn required_text(value: &Value) -> Result<&str, ProviderError> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| decode_error("missing OpenAI tool identity"))
}
