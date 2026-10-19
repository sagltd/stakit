//! Validate fields that the known event mappings consume; ignore future types.
use super::decode_error;
use crate::ProviderError;
use serde_json::Value;

fn object<'a>(value: &'a Value, field: &str) -> Result<&'a Value, ProviderError> {
    value
        .get(field)
        .filter(|value| value.is_object())
        .ok_or_else(|| decode_error(format!("Claude streaming event requires {field} object")))
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| decode_error(format!("Claude streaming event requires {field} string")))
}

pub(super) fn index(event: &Value) -> Result<u64, ProviderError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| decode_error("Claude streaming event requires a nonnegative block index"))
}

pub(super) fn validate(event: &Value) -> Result<&str, ProviderError> {
    let kind = text(event, "type")?;
    match kind {
        "message_start" => {
            let message = object(event, "message")?;
            let tokens = object(message, "usage")?;
            super::super::usage::validate(tokens)?;
            if tokens.get("input_tokens").and_then(Value::as_u64).is_none()
                || tokens
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .is_none()
            {
                return Err(decode_error(
                    "Claude message_start requires input and output usage",
                ));
            }
        }
        "content_block_start" => {
            index(event)?;
            let block = object(event, "content_block")?;
            match text(block, "type")? {
                "tool_use" => {
                    if text(block, "id")?.is_empty() || text(block, "name")?.is_empty() {
                        return Err(decode_error(
                            "Claude tool block requires nonempty id and name",
                        ));
                    }
                    object(block, "input")?;
                }
                "text" => {
                    text(block, "text")?;
                }
                "thinking" => {
                    text(block, "thinking")?;
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            index(event)?;
            let delta = object(event, "delta")?;
            let field = match text(delta, "type")? {
                "text_delta" => Some("text"),
                "thinking_delta" => Some("thinking"),
                "signature_delta" => Some("signature"),
                "input_json_delta" => Some("partial_json"),
                _ => None,
            };
            if let Some(field) = field {
                text(delta, field)?;
            }
        }
        "content_block_stop" => {
            index(event)?;
        }
        "message_delta" => {
            let delta = object(event, "delta")?;
            let tokens = object(event, "usage")?;
            super::super::usage::validate(tokens)?;
            if tokens
                .get("output_tokens")
                .and_then(Value::as_u64)
                .is_none()
            {
                return Err(decode_error("Claude message_delta requires output usage"));
            }
            for field in ["stop_reason", "stop_sequence"] {
                if delta
                    .get(field)
                    .is_some_and(|value| !value.is_null() && !value.is_string())
                {
                    return Err(decode_error(
                        "Claude message_delta contains an invalid stop field",
                    ));
                }
            }
        }
        "error" => {
            let error = object(event, "error")?;
            text(error, "type")?;
            text(error, "message")?;
        }
        _ => {}
    }
    Ok(kind)
}

pub(super) fn stream_error(event: &Value) -> ProviderError {
    let error = event.get("error");
    let kind = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let status = match kind {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        _ => 500,
    };
    ProviderError::Api {
        status,
        kind: kind.to_owned(),
        message: error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Claude streaming request failed")
            .to_owned(),
        retry_after: None,
    }
}
