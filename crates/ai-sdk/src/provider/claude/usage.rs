//! Validate provider counters before missing fields can become mapped zeros.
use super::{map_usage, sse::decode_error};
use crate::{ProviderError, Usage};
use serde_json::Value;

pub(super) fn validate_totals(usage: &Usage) -> Result<(), ProviderError> {
    if usage.cache_create_1h_tokens > usage.cache_create_tokens
        || usage.reasoning_tokens > usage.output_tokens
        || usage
            .input_tokens
            .checked_add(usage.cache_create_tokens)
            .and_then(|total| total.checked_add(usage.cache_read_tokens))
            .is_none()
    {
        return Err(decode_error(
            "Claude usage contains inconsistent token totals",
        ));
    }
    Ok(())
}

pub(super) fn validate(value: &Value) -> Result<(), ProviderError> {
    if !value.is_object() {
        return Err(decode_error("Claude usage must be an object"));
    }
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if value
            .get(field)
            .filter(|value| !value.is_null() || matches!(field, "input_tokens" | "output_tokens"))
            .is_some_and(|value| !value.is_u64())
        {
            return Err(decode_error("Claude usage contains an invalid token count"));
        }
    }
    for (group, fields) in [
        (
            "cache_creation",
            &["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"][..],
        ),
        ("output_tokens_details", &["thinking_tokens"][..]),
    ] {
        let Some(details) = value.get(group).filter(|details| !details.is_null()) else {
            continue;
        };
        if !details.is_object() {
            return Err(decode_error("Claude usage details must be an object"));
        }
        for field in fields {
            if details.get(*field).is_some_and(|value| !value.is_u64()) {
                return Err(decode_error("Claude usage contains an invalid token count"));
            }
        }
    }
    let mapped = map_usage(value);
    if value.get("output_tokens").is_some() && mapped.reasoning_tokens > mapped.output_tokens {
        return Err(decode_error("Claude reasoning usage exceeds output tokens"));
    }
    if let Some(total) = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        let five_minute = value
            .get("cache_creation")
            .and_then(|creation| creation.get("ephemeral_5m_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if five_minute
            .checked_add(mapped.cache_create_1h_tokens)
            .is_none_or(|writes| writes > total)
        {
            return Err(decode_error(
                "Claude cache usage exceeds total cache writes",
            ));
        }
    }
    Ok(())
}

pub(super) fn require_complete(value: &Value) -> Result<(), ProviderError> {
    validate(value)?;
    if ["input_tokens", "output_tokens"]
        .iter()
        .any(|field| value.get(*field).and_then(Value::as_u64).is_none())
    {
        return Err(decode_error(
            "Claude response requires input and output usage",
        ));
    }
    validate_totals(&map_usage(value))
}
