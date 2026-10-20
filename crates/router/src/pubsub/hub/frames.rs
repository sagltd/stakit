//! Frames the hub sends to sessions, and the payloads it hands the broker.

use std::borrow::Cow;
use std::fmt::Write as _;

use bytes::Bytes;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

/// Why the hub closes every local holder of a topic (broker `close` payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseKind {
    /// A newer connection claimed the device.
    Superseded,
    /// The server revoked the user / device (`Hub::close_user` / `close_device`).
    Revoked,
}

impl CloseKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::Revoked => "revoked",
        }
    }
}

/// A hub-encoded broker payload, read back on the receiving node.
#[derive(Deserialize)]
pub(crate) struct Incoming<'a> {
    #[serde(borrow, default)]
    pub(crate) name: Option<Cow<'a, str>>,
    /// `Some("null")` for `null` params — absent is `None`.
    #[serde(borrow, default, deserialize_with = "some_raw")]
    pub(crate) params: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub(crate) kick: Option<Cow<'a, str>>,
    #[serde(default)]
    pub(crate) close: Option<CloseKind>,
}

/// Keeps a JSON `null` as a raw value instead of letting `Option` swallow it.
fn some_raw<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(deserializer).map(Some)
}

/// Appends `text` as a JSON string literal.
fn push_json_str(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// `{"name":…,"params":…}` — `params` is already-serialized JSON.
pub(crate) fn publish_payload(name: &str, params: &str) -> Bytes {
    let mut out = String::with_capacity(params.len() + name.len() + 24);
    out.push_str(r#"{"name":"#);
    push_json_str(&mut out, name);
    out.push_str(r#","params":"#);
    out.push_str(params);
    out.push('}');
    Bytes::from(out)
}

/// `{"kick":"<user>"}`
pub(crate) fn kick_payload(user: &str) -> Bytes {
    let mut out = String::from(r#"{"kick":"#);
    push_json_str(&mut out, user);
    out.push('}');
    Bytes::from(out)
}

/// `{"close":"superseded" | "revoked"}`
pub(crate) fn close_payload(kind: CloseKind) -> Bytes {
    Bytes::from(format!(r#"{{"close":"{}"}}"#, kind.as_str()))
}

/// The publish frame sent to every subscriber, built in one buffer.
pub(crate) fn publish_frame(topic: &str, name: &str, params: &str, epoch: u64, seq: u64) -> Bytes {
    let mut out = String::with_capacity(params.len() + topic.len() + name.len() + 96);
    out.push_str(r#"{"kind":"publish","topic":"#);
    push_json_str(&mut out, topic);
    out.push_str(r#","name":"#);
    push_json_str(&mut out, name);
    let _ = write!(out, r#","seq":{seq},"epoch":{epoch},"params":"#);
    out.push_str(params);
    out.push('}');
    Bytes::from(out)
}

/// `{"kind":"reset","topic":…,"epoch":…,"seq":0}`
pub(crate) fn reset_frame(topic: &str, epoch: u64) -> Bytes {
    let mut out = String::from(r#"{"kind":"reset","topic":"#);
    push_json_str(&mut out, topic);
    let _ = write!(out, r#","epoch":{epoch},"seq":0}}"#);
    Bytes::from(out)
}

/// `{"kind":"unsubscribed","topic":…,"reason":…}`
pub(crate) fn unsubscribed_frame(topic: &str, reason: &str) -> Bytes {
    let mut out = String::from(r#"{"kind":"unsubscribed","topic":"#);
    push_json_str(&mut out, topic);
    out.push_str(r#","reason":"#);
    push_json_str(&mut out, reason);
    out.push('}');
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn parse(bytes: &Bytes) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn publish_frame_escapes_topic_and_name() {
        let frame = publish_frame("a\"b\\c\n\u{1}", "n\"", "[1]", 3, 4);
        assert_eq!(
            parse(&frame),
            json!({ "kind": "publish", "topic": "a\"b\\c\n\u{1}", "name": "n\"", "seq": 4, "epoch": 3, "params": [1] })
        );
    }

    #[test]
    fn payload_roundtrips_including_null_params() {
        let payload = publish_payload("x", "null");
        let incoming: Incoming<'_> = serde_json::from_slice(&payload).unwrap();
        assert_eq!(incoming.name.as_deref(), Some("x"));
        assert_eq!(incoming.params.map(RawValue::get), Some("null"));
    }

    #[test]
    fn control_payloads_parse() {
        let kick = kick_payload("u\"1");
        let kick: Incoming<'_> = serde_json::from_slice(&kick).unwrap();
        assert_eq!(kick.kick.as_deref(), Some("u\"1"));
        assert!(kick.params.is_none());
        let close = close_payload(CloseKind::Revoked);
        let close: Incoming<'_> = serde_json::from_slice(&close).unwrap();
        assert_eq!(close.close, Some(CloseKind::Revoked));
    }

    #[test]
    fn reset_and_unsubscribed_frames_are_json() {
        assert_eq!(
            parse(&reset_frame("t", 9)),
            json!({ "kind": "reset", "topic": "t", "epoch": 9, "seq": 0 })
        );
        assert_eq!(
            parse(&unsubscribed_frame("t", "kicked")),
            json!({ "kind": "unsubscribed", "topic": "t", "reason": "kicked" })
        );
    }
}
