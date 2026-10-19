//! Preserve known counters when compatible endpoints report usage incrementally.
use super::response::decode_error;
use crate::ProviderError;
use serde_json::{Map, Value};

pub(super) fn merge_usage(current: &mut Value, update: &Value) -> Result<(), ProviderError> {
    if !update.is_object() {
        return Err(decode_error("invalid OpenAI usage"));
    }
    if current.is_null() {
        *current = Value::Object(Map::new());
    }
    let was_complete = ["prompt_tokens", "completion_tokens"]
        .iter()
        .all(|key| current.get(*key).and_then(Value::as_u64).is_some());
    let updates_counter = ["prompt_tokens", "completion_tokens"]
        .iter()
        .any(|key| update.get(*key).and_then(Value::as_u64).is_some());
    // A previously checked total describes an earlier cumulative snapshot.
    // Preserve an unchecked partial total until both counters become known.
    if was_complete && updates_counter && update.get("total_tokens").is_none() {
        if let Some(fields) = current.as_object_mut() {
            fields.remove("total_tokens");
        }
    }
    for key in ["prompt_tokens", "completion_tokens"] {
        merge_counter(current, update, key)?;
    }
    if let Some(total) = update.get("total_tokens") {
        if total.as_u64().is_none() {
            return Err(decode_error("invalid OpenAI total usage counter"));
        }
        current["total_tokens"] = total.clone();
    }
    for (group, keys) in [
        (
            "prompt_tokens_details",
            ["cached_tokens", "cache_write_tokens"],
        ),
        (
            "completion_tokens_details",
            ["reasoning_tokens", "audio_tokens"],
        ),
    ] {
        if let Some(detail) = update.get(group).filter(|value| !value.is_null()) {
            if !detail.is_object() {
                return Err(decode_error("invalid OpenAI usage details"));
            }
            if current.get(group).is_none() {
                current[group] = Value::Object(Map::new());
            }
            for key in keys {
                merge_counter(&mut current[group], detail, key)?;
            }
        }
    }
    validate_subsets(current)?;
    validate_reported_total(current)
}

fn validate_subsets(value: &Value) -> Result<(), ProviderError> {
    let prompt = value.get("prompt_tokens").and_then(Value::as_u64);
    let output = value.get("completion_tokens").and_then(Value::as_u64);
    let detail = |group: &str, field: &str| {
        value
            .get(group)
            .and_then(|details| details.get(field))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cached = detail("prompt_tokens_details", "cached_tokens");
    let written = detail("prompt_tokens_details", "cache_write_tokens");
    if prompt.is_some_and(|total| {
        cached
            .checked_add(written)
            .is_none_or(|cache| cache > total)
    }) || output
        .is_some_and(|total| detail("completion_tokens_details", "reasoning_tokens") > total)
    {
        return Err(decode_error(
            "OpenAI usage contains inconsistent token totals",
        ));
    }
    Ok(())
}

fn validate_reported_total(value: &Value) -> Result<(), ProviderError> {
    let Some(total) = value.get("total_tokens").and_then(Value::as_u64) else {
        return Ok(());
    };
    let (Some(prompt), Some(output)) = (
        value.get("prompt_tokens").and_then(Value::as_u64),
        value.get("completion_tokens").and_then(Value::as_u64),
    ) else {
        return Ok(());
    };
    if prompt.checked_add(output) != Some(total) {
        return Err(decode_error(
            "OpenAI total usage does not match prompt and completion tokens",
        ));
    }
    Ok(())
}

pub(super) fn require_complete(value: &Value) -> Result<(), ProviderError> {
    if ["prompt_tokens", "completion_tokens"]
        .iter()
        .any(|field| value.get(*field).and_then(Value::as_u64).is_none())
    {
        return Err(decode_error(
            "OpenAI response requires prompt and completion usage",
        ));
    }
    validate_subsets(value)?;
    validate_reported_total(value)
}

fn merge_counter(current: &mut Value, update: &Value, key: &str) -> Result<(), ProviderError> {
    if let Some(value) = update.get(key).filter(|value| !value.is_null()) {
        let observed = value
            .as_u64()
            .ok_or_else(|| decode_error("invalid OpenAI usage counter"))?;
        if matches!(key, "prompt_tokens" | "completion_tokens")
            && current
                .get(key)
                .and_then(Value::as_u64)
                .is_some_and(|previous| observed < previous)
        {
            return Err(decode_error("OpenAI cumulative token usage decreased"));
        }
        current[key] = value.clone();
    }
    Ok(())
}
