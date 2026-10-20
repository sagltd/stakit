//! Signed NOTIFY envelopes shared by every node.
//!
//! `{"o":<origin>,"n":<counter>,"t":<unix ms>,"m":"<hex hmac>","b":<body>}` —
//! the MAC is HMAC-SHA256 over `o.n.t.` followed by the exact body bytes, so
//! the receiver verifies before it parses the body. `n` is a per-node counter:
//! it makes identical payloads distinct (Postgres collapses duplicate
//! notifications within a transaction) and lets receivers drop replays.

use std::borrow::Cow;
use std::fmt::Write as _;

use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::Sha256;
use stakit_router::Error;

/// Postgres rejects NOTIFY payloads of 8000 bytes or more.
pub(crate) const MAX_NOTIFY_BYTES: usize = 7999;

/// Envelopes older (or newer) than this are rejected.
pub(crate) const MAX_SKEW_MS: u64 = 30_000;

/// Longest possible envelope minus its body: field names + three `u64`s + MAC.
const ENVELOPE_OVERHEAD: usize = r#"{"o":,"n":,"t":,"m":"","b":}"#.len() + 3 * 20 + 64;

type HmacSha256 = Hmac<Sha256>;

/// The signed part of a message.
#[derive(Serialize, Deserialize)]
pub(crate) enum Body<'a> {
    /// A publish (or control message) for `topic`; `p` is the hub's payload.
    #[serde(rename = "p")]
    Publish {
        #[serde(borrow)]
        topic: Cow<'a, str>,
        #[serde(borrow)]
        p: &'a RawValue,
    },
    /// A cross-node device call.
    #[serde(rename = "c")]
    Call {
        id: u64,
        #[serde(borrow)]
        topic: Cow<'a, str>,
        #[serde(borrow)]
        p: &'a RawValue,
    },
    /// The answer to call `id` from node `to`.
    #[serde(rename = "r")]
    Reply {
        to: u64,
        id: u64,
        ok: Option<Value>,
        err: Option<Failure>,
    },
}

/// An error carried back in a [`Body::Reply`].
#[derive(Serialize, Deserialize)]
pub(crate) struct Failure {
    pub(crate) code: u16,
    pub(crate) message: String,
}

/// A verified envelope: who sent it, its counter, and the body.
pub(crate) struct Opened<'a> {
    pub(crate) origin: u64,
    pub(crate) counter: u64,
    pub(crate) body: Body<'a>,
}

#[derive(Deserialize)]
struct Outer<'a> {
    o: u64,
    n: u64,
    t: u64,
    #[serde(borrow)]
    m: Cow<'a, str>,
    #[serde(borrow)]
    b: &'a RawValue,
}

/// The bus key (at least 32 bytes), as a ready-keyed MAC to clone per use.
#[derive(Clone)]
pub(crate) struct Key(HmacSha256);

impl Key {
    pub(crate) fn new(secret: &[u8]) -> Result<Self, Error> {
        if secret.len() < 32 {
            return Err(Error::internal(
                "pub/sub bus secret must be at least 32 bytes",
            ));
        }
        HmacSha256::new_from_slice(secret)
            .map(Self)
            .map_err(|_| Error::internal("invalid pub/sub bus secret"))
    }

    fn mac(&self, origin: u64, counter: u64, time: u64, body: &str) -> HmacSha256 {
        let mut mac = self.0.clone();
        mac.update(format!("{origin}.{counter}.{time}.").as_bytes());
        mac.update(body.as_bytes());
        mac
    }
}

/// The full envelope length for a publish of `payload` on `topic`.
pub(crate) fn publish_len(topic: &str, payload: &[u8]) -> usize {
    let topic = serde_json::to_string(topic).map_or(usize::MAX / 2, |text| text.len());
    ENVELOPE_OVERHEAD + r#"{"p":{"topic":,"p":}}"#.len() + topic + payload.len()
}

/// The size check every outgoing message passes.
pub(crate) fn check_len(len: usize) -> Result<(), Error> {
    if len > MAX_NOTIFY_BYTES {
        return Err(Error::coded(
            413,
            "payload_too_large",
            "pub/sub payload exceeds the Postgres NOTIFY limit (8000 bytes); publish ids and fetch the data instead",
        ));
    }
    Ok(())
}

/// Signs `body` into an envelope, enforcing the NOTIFY size limit.
pub(crate) fn seal(
    key: &Key,
    origin: u64,
    counter: u64,
    time: u64,
    body: &Body<'_>,
) -> Result<String, Error> {
    let body = serde_json::to_string(body).map_err(|e| Error::encode(&e))?;
    check_len(ENVELOPE_OVERHEAD + body.len())?;
    let tag = key
        .mac(origin, counter, time, &body)
        .finalize()
        .into_bytes();
    let mut hex = String::with_capacity(64);
    for byte in tag {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(format!(
        r#"{{"o":{origin},"n":{counter},"t":{time},"m":"{hex}","b":{body}}}"#
    ))
}

/// Verifies an envelope (signature in constant time, then freshness) and
/// parses its body. `None` for anything unsigned, forged, stale or malformed.
pub(crate) fn open<'a>(key: &Key, text: &'a str, now: u64) -> Option<Opened<'a>> {
    let outer: Outer<'a> = serde_json::from_str(text).ok()?;
    let tag = decode_hex(&outer.m)?;
    let body = outer.b.get();
    key.mac(outer.o, outer.n, outer.t, body)
        .verify_slice(&tag)
        .ok()?;
    if now.abs_diff(outer.t) > MAX_SKEW_MS {
        return None;
    }
    let body = serde_json::from_str(body).ok()?;
    Some(Opened {
        origin: outer.o,
        counter: outer.n,
        body,
    })
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

/// The hub's JSON payload as a raw value (it is always valid JSON).
pub(crate) fn raw(payload: &[u8]) -> Result<&RawValue, Error> {
    serde_json::from_slice(payload).map_err(|e| Error::decode(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn key() -> Key {
        Key::new(&[7; 32]).unwrap()
    }

    fn sealed(payload: &str) -> String {
        let p = raw(payload.as_bytes()).unwrap();
        let body = Body::Publish {
            topic: "room:id=1".into(),
            p,
        };
        seal(&key(), 7, 1, NOW, &body).unwrap()
    }

    #[test]
    fn sealed_publish_opens() {
        let text = sealed(r#"{"name":"x","params":1}"#);
        let opened = open(&key(), &text, NOW).unwrap();
        assert_eq!((opened.origin, opened.counter), (7, 1));
        let Body::Publish { topic, p } = opened.body else {
            panic!("not a publish");
        };
        assert_eq!(topic, "room:id=1");
        assert_eq!(p.get(), r#"{"name":"x","params":1}"#);
    }

    #[test]
    fn publish_len_matches_or_exceeds_the_real_envelope() {
        let payload = r#"{"name":"x","params":"hello"}"#;
        let text = sealed(payload);
        assert!(publish_len("room:id=1", payload.as_bytes()) >= text.len());
    }

    #[test]
    fn unsigned_and_forged_envelopes_are_rejected() {
        let unsigned = r#"{"o":1,"b":{"p":{"topic":"t","p":1}}}"#;
        assert!(open(&key(), unsigned, NOW).is_none());
        let other_key = Key::new(&[8; 32]).unwrap();
        assert!(open(&other_key, &sealed("1"), NOW).is_none());
    }

    #[test]
    fn tampered_body_or_header_is_rejected() {
        let text = sealed(r#""a""#);
        assert!(open(&key(), &text.replace(r#""a""#, r#""b""#), NOW).is_none());
        assert!(open(&key(), &text.replace(r#""o":7"#, r#""o":8"#), NOW).is_none());
    }

    #[test]
    fn stale_or_future_envelopes_are_rejected() {
        let text = sealed("1");
        assert!(open(&key(), &text, NOW + MAX_SKEW_MS + 1).is_none());
        assert!(open(&key(), &text, NOW - MAX_SKEW_MS - 1).is_none());
        assert!(open(&key(), &text, NOW + MAX_SKEW_MS).is_some());
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let big = format!(r#""{}""#, "x".repeat(MAX_NOTIFY_BYTES));
        let p = raw(big.as_bytes()).unwrap();
        let body = Body::Publish {
            topic: "t".into(),
            p,
        };
        assert_eq!(seal(&key(), 1, 1, NOW, &body).unwrap_err().code, 413);
        assert!(check_len(publish_len("t", big.as_bytes())).is_err());
    }

    #[test]
    fn short_secret_is_refused() {
        assert!(Key::new(&[1; 31]).is_err());
    }

    #[test]
    fn malformed_payload_is_a_decode_error() {
        assert_eq!(raw(b"{").unwrap_err().code, 400);
    }
}
