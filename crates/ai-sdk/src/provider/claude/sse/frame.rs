//! Claude JSON and named-event validation after shared byte framing.
use super::decode_error;
use crate::{ProviderError, provider::sse::Frame};
use serde_json::Value;

pub(super) fn decode(frame: Frame) -> Result<Value, ProviderError> {
    let value: Value = serde_json::from_slice(&frame.data).map_err(|error| {
        decode_error(format!(
            "invalid Claude SSE JSON at line {} column {}",
            error.line(),
            error.column()
        ))
    })?;
    if let Some(name) = frame.event {
        if value.get("type").and_then(Value::as_str) != Some(name.as_str()) {
            return Err(decode_error(
                "Claude SSE event name does not match its data type",
            ));
        }
    }
    Ok(value)
}
