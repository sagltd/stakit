//! Unified, provider-agnostic conversation model.
//!
//! A [`Message`] is one conversation turn. The system prompt is **not** a
//! message — it is a request-level field ([`SystemPrompt`]), matching how both
//! Anthropic (top-level `system`) and `OpenAI` (a leading system/developer
//! message or `instructions`) model it. Tool-call inputs are kept as parsed
//! [`serde_json::Value`]s; each provider's serializer renders them into the
//! vendor wire shape. This keeps illegal states (e.g. a tool result on an
//! assistant turn) unrepresentable.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// A single conversation turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// A user (or tool-result) turn.
    User(Vec<UserContent>),
    /// An assistant (model-produced) turn.
    Assistant(Vec<AssistantContent>),
}

impl Message {
    /// A user turn containing a single text block.
    pub fn user(text: impl Into<Arc<str>>) -> Self {
        Self::User(vec![UserContent::Text(text.into())])
    }

    /// A user turn containing a single text block.
    ///
    /// Alias for [`Message::user`]; kept for compatibility.
    pub fn user_text(text: impl Into<Arc<str>>) -> Self {
        Self::user(text)
    }

    /// An assistant turn containing a single text block.
    pub fn assistant(text: impl Into<Arc<str>>) -> Self {
        Self::Assistant(vec![AssistantContent::Text(text.into())])
    }

    /// An assistant turn containing a single text block.
    ///
    /// Alias for [`Message::assistant`]; kept for compatibility.
    pub fn assistant_text(text: impl Into<Arc<str>>) -> Self {
        Self::assistant(text)
    }
}

/// A content block on a user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserContent {
    /// Plain text.
    Text(Arc<str>),
    /// An image input.
    Image(Image),
    /// The result of a tool call, referencing the originating [`AssistantContent::ToolUse`] by `id`.
    ToolResult {
        /// Correlates with the `id` of the assistant `ToolUse` block.
        id: String,
        /// Result payload (text and/or images).
        content: Vec<ToolResultPart>,
        /// Whether this result represents a tool failure (the model may retry).
        is_error: bool,
    },
}

/// A content block on an assistant turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistantContent {
    /// Plain text.
    Text(Arc<str>),
    /// A request to call a tool, with already-parsed arguments.
    ToolUse {
        /// Unique id for this call; the matching `ToolResult` references it.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed tool arguments (conform to the tool's JSON Schema).
        input: Value,
    },
    /// Model reasoning. Preserved losslessly so it can be replayed verbatim
    /// (Anthropic requires the signature to round-trip before a `tool_use`).
    Thinking(Thinking),
}

/// Model reasoning content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Thinking {
    /// Visible reasoning text, with an optional integrity signature.
    Visible {
        /// The reasoning text.
        text: String,
        /// Provider integrity signature (Anthropic), replayed verbatim.
        signature: Option<String>,
    },
    /// Encrypted/redacted reasoning (opaque blob, replayed verbatim).
    Redacted {
        /// Opaque provider data.
        data: String,
    },
}

/// A part of a tool result's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultPart {
    /// Text output.
    Text(String),
    /// Image output.
    Image(Image),
}

/// A provider-neutral image reference inside a message.
///
/// Prefer [`Image::Url`] / [`Image::FileId`]: base64 is re-sent every turn and
/// bloats both the request and the prompt cache. Each provider adapter maps this
/// to its own wire format at send time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Image {
    /// A public URL the provider fetches.
    Url {
        /// The image URL.
        url: Arc<str>,
    },
    /// An id from a provider Files API (uploaded once, referenced cheaply).
    FileId {
        /// The file identifier.
        file_id: Arc<str>,
    },
    /// Inline base64 bytes; use only when no URL/file id exists.
    Base64 {
        /// MIME type, e.g. `image/png`.
        media_type: Arc<str>,
        /// Raw bytes (cheap to clone).
        data: Bytes,
    },
}

/// The request-level system prompt, with an optional cache breakpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPrompt {
    /// The system prompt text.
    pub text: Arc<str>,
    /// Request a prompt-cache breakpoint after this block (Anthropic; no-op on
    /// providers with automatic caching).
    pub cache: bool,
}

impl From<String> for SystemPrompt {
    fn from(text: String) -> Self {
        Self {
            text: text.into(),
            cache: false,
        }
    }
}

impl From<&str> for SystemPrompt {
    fn from(text: &str) -> Self {
        Self {
            text: text.into(),
            cache: false,
        }
    }
}

impl From<Arc<str>> for SystemPrompt {
    fn from(text: Arc<str>) -> Self {
        Self { text, cache: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_url_is_cheap_clone_and_serializes_as_url() {
        let img = Image::Url {
            url: "https://x/y.png".into(),
        };
        let clone = img.clone();
        assert!(matches!(clone, Image::Url { ref url } if &**url == "https://x/y.png"));
        let json = serde_json::to_value(&img).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "type": "url", "url": "https://x/y.png" })
        );
    }

    #[test]
    fn image_base64_roundtrips() {
        let img = Image::Base64 {
            media_type: "image/png".into(),
            data: bytes::Bytes::from_static(b"\x89PNG"),
        };
        let json = serde_json::to_value(&img).unwrap();
        let back: Image = serde_json::from_value(json).unwrap();
        assert!(matches!(back, Image::Base64 { .. }));
    }

    #[test]
    fn user_text_builds_single_text_block() {
        assert_eq!(
            Message::user_text("hi"),
            Message::User(vec![UserContent::Text("hi".into())])
        );
    }

    #[test]
    fn message_round_trips_through_json() {
        let msg = Message::Assistant(vec![AssistantContent::ToolUse {
            id: "toolu_1".into(),
            name: "get_weather".into(),
            input: serde_json::json!({ "city": "Paris" }),
        }]);
        let text = serde_json::to_string(&msg).expect("serialize");
        let back: Message = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn system_prompt_from_str_has_no_cache_breakpoint() {
        let sp = SystemPrompt::from("you are helpful");
        assert_eq!(&*sp.text, "you are helpful");
        assert!(!sp.cache);
    }

    #[test]
    fn user_message_text_is_arc_backed_and_cheap_to_clone() {
        let m = Message::user("hello");
        let c = m.clone();
        // Both the original and the clone share the same Arc-backed text.
        assert_eq!(m, c);
        assert!(format!("{c:?}").contains("hello"));
    }
}
