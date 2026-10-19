//! Exact per-index content, bounded before any fragment is appended.
use super::sse::decode_error;
use crate::provider::{MAX_STREAM_BLOCK_BYTES, MAX_STREAM_RESPONSE_BYTES};
use crate::{AssistantContent, ProviderError, StreamEvent, Thinking};
use serde_json::Value;

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| decode_error(format!("Claude block requires {field} string")))
}

fn reserve(
    current: usize,
    removed: usize,
    added: usize,
    total: &mut usize,
) -> Result<(), ProviderError> {
    let block = current.saturating_sub(removed).saturating_add(added);
    let response = total.saturating_sub(removed).saturating_add(added);
    if block > MAX_STREAM_BLOCK_BYTES || response > MAX_STREAM_RESPONSE_BYTES {
        return Err(decode_error(
            "Claude response content exceeded its byte limit",
        ));
    }
    *total = response;
    Ok(())
}

fn tool_input(
    initial: Option<Value>,
    json: Option<String>,
) -> Result<(Value, String), ProviderError> {
    if let Some(arguments) = json {
        let input = serde_json::from_str::<Value>(&arguments)
            .map_err(|_| decode_error("Claude tool arguments are not valid JSON"))?;
        return Ok((input, arguments));
    }
    let input = initial.ok_or_else(|| decode_error("Claude tool input is missing"))?;
    let arguments = input.to_string();
    Ok((input, arguments))
}

pub(super) enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Redacted(String),
    Tool {
        id: String,
        name: String,
        initial: Option<Value>,
        json: Option<String>,
    },
}

impl Block {
    pub(super) fn start(block: &Value) -> Result<Option<Self>, ProviderError> {
        Ok(match text(block, "type")? {
            "text" => Some(Self::Text(text(block, "text")?.to_owned())),
            "thinking" => Some(Self::Thinking {
                text: text(block, "thinking")?.to_owned(),
                signature: block
                    .get("signature")
                    .map(|value| {
                        value.as_str().ok_or_else(|| {
                            decode_error("Claude thinking signature must be a string")
                        })
                    })
                    .transpose()?
                    .map(ToOwned::to_owned),
            }),
            "redacted_thinking" => Some(Self::Redacted(text(block, "data")?.to_owned())),
            "tool_use" => Some(Self::Tool {
                id: text(block, "id")?.to_owned(),
                name: text(block, "name")?.to_owned(),
                initial: Some(
                    block
                        .get("input")
                        .filter(|value| value.is_object())
                        .ok_or_else(|| decode_error("Claude tool block requires input object"))?
                        .clone(),
                ),
                json: None,
            }),
            _ => None,
        })
    }

    pub(super) fn bytes(&self) -> usize {
        match self {
            Self::Text(text) | Self::Redacted(text) => text.len(),
            Self::Thinking { text, signature } => text
                .len()
                .saturating_add(signature.as_ref().map_or(0, String::len)),
            Self::Tool {
                id,
                name,
                initial,
                json,
            } => id
                .len()
                .saturating_add(name.len())
                .saturating_add(json.as_ref().map_or_else(
                    || initial.as_ref().map_or(0, |input| input.to_string().len()),
                    String::len,
                )),
        }
    }

    pub(super) fn initial_events(&self) -> Vec<StreamEvent> {
        match self {
            Self::Text(text) if !text.is_empty() => vec![StreamEvent::TextDelta(text.clone())],
            Self::Thinking { text, signature } => {
                let mut events = Vec::new();
                if !text.is_empty() {
                    events.push(StreamEvent::ReasoningDelta(text.clone()));
                }
                if let Some(signature) =
                    signature.as_ref().filter(|signature| !signature.is_empty())
                {
                    events.push(StreamEvent::SignatureDelta(signature.clone()));
                }
                events
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn delta(
        &mut self,
        delta: &Value,
        total: &mut usize,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let current = self.bytes();
        match (self, text(delta, "type")?) {
            (Self::Text(buffer), "text_delta") => {
                let part = text(delta, "text")?;
                reserve(current, 0, part.len(), total)?;
                buffer.push_str(part);
                Ok(vec![StreamEvent::TextDelta(part.to_owned())])
            }
            (Self::Thinking { text: buffer, .. }, "thinking_delta") => {
                let part = text(delta, "thinking")?;
                reserve(current, 0, part.len(), total)?;
                buffer.push_str(part);
                Ok(vec![StreamEvent::ReasoningDelta(part.to_owned())])
            }
            (Self::Thinking { signature, .. }, "signature_delta") => {
                let part = text(delta, "signature")?;
                reserve(current, 0, part.len(), total)?;
                signature.get_or_insert_with(String::new).push_str(part);
                Ok(vec![StreamEvent::SignatureDelta(part.to_owned())])
            }
            (Self::Tool { initial, json, .. }, "input_json_delta") => {
                let part = text(delta, "partial_json")?;
                if part.is_empty() {
                    return Ok(Vec::new());
                }
                let removed = if json.is_none() {
                    if initial.as_ref().is_some_and(|input| {
                        input.as_object().is_none_or(|object| !object.is_empty())
                    }) {
                        return Err(decode_error(
                            "Claude tool input conflicts with streamed arguments",
                        ));
                    }
                    initial.as_ref().map_or(0, |input| input.to_string().len())
                } else {
                    0
                };
                reserve(current, removed, part.len(), total)?;
                *initial = None;
                json.get_or_insert_with(String::new).push_str(part);
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    pub(super) fn finish(self) -> Result<(AssistantContent, Option<StreamEvent>), ProviderError> {
        Ok(match self {
            Self::Text(text) => (AssistantContent::Text(text.into()), None),
            Self::Thinking { text, signature } => (
                AssistantContent::Thinking(Thinking::Visible {
                    text: text.into(),
                    signature: signature.map(Into::into),
                }),
                None,
            ),
            Self::Redacted(data) => (
                AssistantContent::Thinking(Thinking::Redacted { data: data.into() }),
                None,
            ),
            Self::Tool {
                id,
                name,
                initial,
                json,
            } => {
                let (input, arguments) = tool_input(initial, json)?;
                let call = StreamEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments,
                };
                (
                    AssistantContent::ToolUse {
                        id: id.into(),
                        name: name.into(),
                        input,
                    },
                    Some(call),
                )
            }
        })
    }
}
