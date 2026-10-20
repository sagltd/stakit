//! Typed channels: a stable name + serializable key fields → a topic string.

use std::fmt::Write as _;
use std::future::Future;
use std::marker::PhantomData;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use stakit_model::Model;

use crate::{Cx, Error};

/// Longest topic accepted (`name:key…`, after escaping). Bounds index memory and
/// keeps broker payloads small.
pub const MAX_TOPIC_BYTES: usize = 256;

/// A pub/sub channel: a stable [`NAME`](Channel::NAME) plus the key fields that
/// pick one instance of it (e.g. a conversation id).
///
/// The topic is `NAME:field=value:…` — fields in declaration order, strings
/// as-is, numbers/booleans tagged with `#` (so `"7"` and `7` differ), and
/// `% : = #` percent-escaped so two keys never collide. Publishing only needs
/// this trait; letting clients subscribe needs [`Authorize`] too.
///
/// ```ignore
/// #[derive(Model, Serialize, Deserialize)]
/// struct Conversation { id: String }                  // topic "conversation:id=…"
/// impl Channel for Conversation { const NAME: &'static str = "conversation"; }
/// ```
pub trait Channel: Serialize {
    /// Stable channel name — the topic prefix and the name clients subscribe by.
    /// `user` and `device` are reserved for the built-in channels.
    const NAME: &'static str;

    /// The topic string for this channel instance.
    ///
    /// # Errors
    /// `400` if a key field is not a scalar or the topic exceeds
    /// [`MAX_TOPIC_BYTES`].
    fn topic(&self) -> Result<String, Error> {
        encode_topic(Self::NAME, self)
    }
}

/// Subscribe-side authorization for a [`Channel`].
///
/// Register it with [`Builder::channel`](crate::Builder::channel) and clients
/// may `subscribe` to it. Checked on every subscribe; `Ok(false)` is answered
/// with `403`.
pub trait Authorize<G, R>: Channel + Model + DeserializeOwned + Send + Sync + 'static {
    /// Whether the caller (`cx.req`) may subscribe to this channel instance.
    fn authorize<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
    ) -> impl Future<Output = Result<bool, Error>> + Send + 'a;
}

/// Built-in per-user channel (`user:id=<id>`). Every authenticated session with
/// a user id is subscribed to it automatically; clients cannot subscribe by hand.
#[derive(Serialize)]
pub struct UserChannel<'a> {
    /// The user id.
    pub id: &'a str,
}

impl Channel for UserChannel<'_> {
    const NAME: &'static str = "user";
}

/// Built-in per-device channel (`device:user=<user>:id=<id>`).
///
/// Bound to its owner and held by at most one connection. Auto-subscribed like
/// [`UserChannel`]; the target of [`Cx::client_call_device`](crate::Cx::client_call_device).
#[derive(Serialize)]
pub struct DeviceChannel<'a> {
    /// The owning user id.
    pub user: &'a str,
    /// The device id.
    pub id: &'a str,
}

impl Channel for DeviceChannel<'_> {
    const NAME: &'static str = "device";
}

/// Names of the built-in channels, which [`Builder::channel`](crate::Builder::channel) refuses.
pub(crate) const RESERVED_NAMES: [&str; 2] = [UserChannel::NAME, DeviceChannel::NAME];

/// Encodes `name` + `key`'s scalar fields into a topic string. Field order is
/// the serialization (declaration) order, read through an order-preserving map.
pub(crate) fn encode_topic(name: &str, key: &(impl Serialize + ?Sized)) -> Result<String, Error> {
    let text = serde_json::to_string(key).map_err(|e| Error::encode(&e))?;
    let mut topic = String::new();
    push_escaped(&mut topic, name);
    if text.starts_with('{') {
        let fields: IndexMap<String, Value> =
            serde_json::from_str(&text).map_err(|e| Error::decode(&e))?;
        for (field, value) in &fields {
            topic.push(':');
            push_escaped(&mut topic, field);
            topic.push('=');
            push_scalar(&mut topic, value)?;
        }
    } else {
        let value: Value = serde_json::from_str(&text).map_err(|e| Error::decode(&e))?;
        topic.push(':');
        push_scalar(&mut topic, &value)?;
    }
    if topic.len() > MAX_TOPIC_BYTES {
        return Err(Error::coded(400, "bad_request", "channel key too long"));
    }
    Ok(topic)
}

fn push_escaped(topic: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '%' => topic.push_str("%25"),
            ':' => topic.push_str("%3A"),
            '=' => topic.push_str("%3D"),
            '#' => topic.push_str("%23"),
            other => topic.push(other),
        }
    }
}

fn push_scalar(topic: &mut String, value: &Value) -> Result<(), Error> {
    match value {
        Value::String(text) => push_escaped(topic, text),
        Value::Number(number) => {
            let _ = write!(topic, "#{number}");
        }
        Value::Bool(flag) => {
            let _ = write!(topic, "#{flag}");
        }
        Value::Null | Value::Array(_) | Value::Object(_) => {
            return Err(Error::coded(
                400,
                "bad_request",
                "channel key fields must be strings, numbers or booleans",
            ));
        }
    }
    Ok(())
}

/// A validated, authorized subscribe request: its topic, and the topic's
/// revocation mark read *before* `authorize` ran (see [`Hub::revocation_mark`]).
///
/// [`Hub::revocation_mark`]: crate::Hub
pub(crate) struct Resolved {
    pub(crate) topic: String,
    pub(crate) mark: u64,
}

/// Type-erased registered channel: key JSON → validated, authorized topic.
pub(crate) trait ErasedChannel<G, R>: Send + Sync {
    fn resolve<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        key: Value,
    ) -> BoxFuture<'a, Result<Resolved, Error>>;
}

/// Erases a registered [`Authorize`] channel type.
pub(crate) struct Registered<C>(pub(crate) PhantomData<fn() -> C>);

impl<G, R, C> ErasedChannel<G, R> for Registered<C>
where
    C: Authorize<G, R>,
    G: Send + Sync + 'static,
    R: Send + Sync + 'static,
{
    fn resolve<'a>(
        &'a self,
        cx: &'a Cx<G, R>,
        key: Value,
    ) -> BoxFuture<'a, Result<Resolved, Error>> {
        Box::pin(async move {
            let channel: C = serde_json::from_value(key).map_err(|e| Error::decode(&e))?;
            channel.validate().map_err(Error::validation)?;
            let topic = channel.topic()?;
            // Read before authorizing: a kick that lands while `authorize`
            // runs moves the mark, and the hub then refuses the subscribe.
            let mark = cx.hub.revocation_mark(&topic);
            if !channel.authorize(cx).await? {
                return Err(Error::coded(403, "forbidden", "not allowed to subscribe"));
            }
            Ok(Resolved { topic, mark })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Pair {
        b: String,
        a: String,
    }

    impl Channel for Pair {
        const NAME: &'static str = "pair";
    }

    #[derive(Serialize)]
    struct Nested {
        inner: Vec<u8>,
    }

    impl Channel for Nested {
        const NAME: &'static str = "nested";
    }

    #[derive(Serialize)]
    struct Optional {
        #[serde(skip_serializing_if = "Option::is_none")]
        a: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        b: Option<String>,
    }

    impl Channel for Optional {
        const NAME: &'static str = "optional";
    }

    #[derive(Serialize)]
    struct Number {
        id: u64,
    }

    #[derive(Serialize)]
    struct Text {
        id: String,
    }

    fn pair(b: &str, a: &str) -> Pair {
        Pair {
            b: b.to_owned(),
            a: a.to_owned(),
        }
    }

    #[test]
    fn topic_keeps_declaration_order_with_field_names() {
        // `b` is declared first; a sorted map would put `a` first.
        assert_eq!(pair("x", "y").topic().unwrap(), "pair:b=x:a=y");
    }

    #[test]
    fn built_in_channels_use_their_prefix() {
        assert_eq!(UserChannel { id: "u1" }.topic().unwrap(), "user:id=u1");
        let device = DeviceChannel {
            user: "u1",
            id: "d1",
        };
        assert_eq!(device.topic().unwrap(), "device:user=u1:id=d1");
    }

    #[test]
    fn separators_in_keys_cannot_collide() {
        assert_ne!(
            pair("x:a=y", "z").topic().unwrap(),
            pair("x", "y:a=z").topic().unwrap()
        );
        assert_eq!(
            pair("x:y", "%=#").topic().unwrap(),
            "pair:b=x%3Ay:a=%25%3D%23"
        );
    }

    #[test]
    fn numbers_and_strings_do_not_collide() {
        let number = encode_topic("n", &Number { id: 7 }).unwrap();
        let text = encode_topic("n", &Text { id: "7".to_owned() }).unwrap();
        assert_eq!(number, "n:id=#7");
        assert_eq!(text, "n:id=7");
        assert_eq!(encode_topic("b", &true).unwrap(), "b:#true");
        assert_ne!(
            encode_topic("b", &true).unwrap(),
            encode_topic("b", "true").unwrap()
        );
    }

    #[test]
    fn skipped_optional_fields_do_not_collide() {
        let a = Optional {
            a: Some("x".to_owned()),
            b: None,
        };
        let b = Optional {
            a: None,
            b: Some("x".to_owned()),
        };
        assert_ne!(a.topic().unwrap(), b.topic().unwrap());
    }

    #[test]
    fn name_separators_are_escaped() {
        assert_eq!(encode_topic("a:b", "c").unwrap(), "a%3Ab:c");
    }

    #[test]
    fn non_scalar_key_is_rejected() {
        let error = Nested { inner: vec![1] }.topic().unwrap_err();
        assert_eq!(error.code, 400);
    }

    #[test]
    fn oversized_topic_is_rejected() {
        let error = pair(&"x".repeat(MAX_TOPIC_BYTES), "y").topic().unwrap_err();
        assert_eq!(error.code, 400);
    }
}
