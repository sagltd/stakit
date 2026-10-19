//! Object-shaped arguments for tools that take no model-supplied parameters.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use stakit_model::{JsonSchema, Validate, ValidationErrors};

/// Empty tool arguments, serialized as `{}` rather than JSON `null`.
///
/// Used by `#[tool]` for functions without an arguments parameter, including
/// context-only functions. Unknown fields and non-object values are rejected
/// before the tool runs. Handwritten tools may use this type as `Tool::Args`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct NoToolArgs {}

impl<'de> Deserialize<'de> for NoToolArgs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A derived empty struct also accepts an empty sequence. Tool arguments
        // must match the advertised object schema exactly, so require a map.
        let fields = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        if fields.is_empty() {
            Ok(Self {})
        } else {
            Err(serde::de::Error::custom("tool accepts no argument fields"))
        }
    }
}

impl JsonSchema for NoToolArgs {
    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false,
        })
    }
}

impl Validate for NoToolArgs {
    fn validate(&self) -> Result<(), ValidationErrors> {
        Ok(())
    }
}
