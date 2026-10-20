//! The [`Hub`]: this node's topic → subscriber index and fan-out.
#![expect(
    clippy::significant_drop_tightening,
    reason = "`conns` is held across the whole step on purpose (lock order: conns → shard)"
)]

mod frames;
mod state;
#[cfg(test)]
mod tests;

use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt as _;
use hashbrown::HashMap;
use serde_json::Value;
use tokio::sync::mpsc;

pub(crate) use state::{Closer, ConnId, Cursor, Subscribe, signal};

use self::frames::{CloseKind, Incoming, close_payload, kick_payload, publish_payload};
use self::state::{Conn, Inner, gone, lock};
use super::broker::{Broker, InMemoryBroker};
use crate::client::{ClientHandle, DEFAULT_CLIENT_CALL_TIMEOUT};
use crate::session::{CloseReason, Identity};
use crate::{Channel, ClientAction, DeviceChannel, Error, UserChannel};

/// This node's pub/sub hub: which local connections listen on which topic, and
/// the [`Broker`] that links it to other nodes. Cheap to clone (an `Arc`).
///
/// Delivery is **at-most-once**. Each topic carries a monotonic `seq` and an
/// `epoch` (changes whenever the topic's history resets on this node — new topic
/// state, broker reconnect), so clients detect gaps and resync from their source
/// of truth. A subscriber whose outgoing queue is full is closed, not waited on.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

/// The hub side of a [`Broker`]: hand inbound traffic from other nodes to it.
#[derive(Clone)]
pub struct BrokerSink {
    hub: Weak<Inner>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new(InMemoryBroker::new())
    }
}

impl Hub {
    /// A hub linked to other nodes through `broker` (use [`InMemoryBroker`] for
    /// a single node). Cross-node device calls wait up to
    /// [`DEFAULT_CLIENT_CALL_TIMEOUT`].
    pub fn new(broker: impl Broker) -> Self {
        Self::with_call_timeout(broker, DEFAULT_CLIENT_CALL_TIMEOUT)
    }

    /// Like [`Hub::new`], with the cross-node device call timeout.
    pub fn with_call_timeout(broker: impl Broker, call_timeout: Duration) -> Self {
        let epoch_base = RandomState::new().build_hasher().finish();
        let inner = Arc::new(Inner::new(Arc::new(broker), call_timeout, epoch_base));
        inner.broker.attach(BrokerSink {
            hub: Arc::downgrade(&inner),
        });
        Self { inner }
    }

    /// Publishes client action `A` with `params` to every connection subscribed
    /// to `topic`, on every node (at-most-once).
    ///
    /// The payload is checked against the broker's limits first; then local
    /// subscribers get it; then it is handed to the broker for the other nodes.
    ///
    /// # Errors
    /// `500` if `params` don't serialize, the broker's limit error (e.g. `413`)
    /// — then nobody got it — or the broker's hand-off error, in which case
    /// this node's subscribers already got it but other nodes may not.
    pub async fn publish<A: ClientAction>(
        &self,
        topic: &str,
        params: &A::Params,
    ) -> Result<(), Error> {
        let params = serde_json::to_string(params).map_err(|e| Error::encode(&e))?;
        let payload = publish_payload(A::NAME, &params);
        self.inner.broker.validate(topic, &payload)?;
        self.inner.fan_out(topic, A::NAME, &params);
        self.inner.broker.publish(topic, payload).await
    }

    /// Force-unsubscribes every connection of `user` from `topic` on every node
    /// (e.g. after they leave a conversation). Each gets an
    /// `{ kind: "unsubscribed", topic, reason: "kicked" }` frame. This node is
    /// done before the broker is asked, so a broker failure never leaves a
    /// local subscriber behind; a subscribe whose `authorize` overlapped the
    /// kick is refused and re-authorized.
    ///
    /// # Errors
    /// The broker's error if it cannot reach the other nodes.
    pub async fn kick_user(&self, topic: &str, user: &str) -> Result<(), Error> {
        self.inner.kick_local(topic, user);
        self.inner.broker.publish(topic, kick_payload(user)).await
    }

    /// Closes every session of `user` on every node (logout, account deleted,
    /// token revoked), with [`CloseReason::Revoked`].
    ///
    /// # Errors
    /// `400` for an invalid id, or the broker's error reaching other nodes
    /// (this node's sessions are closed regardless).
    pub async fn close_user(&self, user: &str) -> Result<(), Error> {
        let topic = UserChannel { id: user }.topic()?;
        let ids: Vec<ConnId> = lock(&self.inner.conns)
            .by_id
            .iter()
            .filter(|(_, conn)| conn.user.as_deref() == Some(user))
            .map(|(id, _)| *id)
            .collect();
        self.inner.close_all(&ids, CloseReason::Revoked);
        let payload = close_payload(CloseKind::Revoked);
        self.inner.broker.publish(&topic, payload).await
    }

    /// Closes the session holding `user`'s `device` on any node, with
    /// [`CloseReason::Revoked`].
    ///
    /// # Errors
    /// As [`Hub::close_user`].
    pub async fn close_device(&self, user: &str, device: &str) -> Result<(), Error> {
        let topic = DeviceChannel { user, id: device }.topic()?;
        self.inner.close_topic(&topic, CloseKind::Revoked);
        let payload = close_payload(CloseKind::Revoked);
        self.inner.broker.publish(&topic, payload).await
    }

    /// Local subscribers of `topic` on this node.
    #[must_use]
    pub fn subscriber_count(&self, topic: &str) -> usize {
        self.inner
            .read_shard(topic)
            .get(topic)
            .map_or(0, |state| state.subscribers.len())
    }

    /// Open connections on this node.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        lock(&self.inner.conns).by_id.len()
    }

    pub(crate) fn register(
        &self,
        tx: mpsc::Sender<Bytes>,
        client: ClientHandle,
        closer: Arc<Closer>,
    ) -> ConnId {
        let id = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        let conn = Conn {
            tx,
            client,
            closer,
            authenticated: false,
            user: None,
            device: None,
            topics: HashMap::new(),
            explicit: 0,
        };
        lock(&self.inner.conns).by_id.insert(id, conn);
        id
    }

    /// Drops a connection and all its subscriptions, then signals `reason`.
    pub(crate) fn close(&self, id: ConnId, reason: CloseReason) {
        self.inner.close(id, reason);
    }

    /// Drops a connection silently (its session is gone).
    pub(crate) fn remove(&self, id: ConnId) {
        self.inner.detach(id);
    }

    /// The topic's revocation mark (read before `authorize`).
    pub(crate) fn revocation_mark(&self, topic: &str) -> u64 {
        self.inner.revocation_mark(topic)
    }

    /// Binds `identity` to a connection and subscribes its `user:` / `device:`
    /// channels — all or nothing. Enforces `max_per_user`; a device is bound to
    /// its user and held by one connection: another user's claim is `409`, the
    /// same user's newer connection supersedes the old one on every node.
    pub(crate) fn authenticate(
        &self,
        id: ConnId,
        identity: &Identity,
        max_per_user: usize,
    ) -> Result<(), Error> {
        let user = identity.user.as_deref();
        let device = identity.device.as_deref();
        let (user_topic, device_topic) = identity_topics(user, device)?;
        let mut slow = Vec::new();
        let superseded = {
            let mut conns = lock(&self.inner.conns);
            let conn = conns.by_id.get(&id).ok_or_else(gone)?;
            if conn.authenticated {
                return Err(already_authenticated());
            }
            if let Some(user) = user
                && conns.per_user.get(user).copied().unwrap_or(0) >= max_per_user
            {
                return Err(Error::coded(
                    429,
                    "too_many_connections",
                    "too many connections for this user",
                ));
            }
            let holder = device.and_then(|device| conns.devices.get(device).copied());
            let holder = holder.filter(|holder| *holder != id);
            if let Some(holder) = holder {
                let holder_user = conns.by_id.get(&holder).and_then(|conn| conn.user.clone());
                if holder_user.as_deref() != user {
                    return Err(Error::coded(
                        409,
                        "device_conflict",
                        "the device is held by another user",
                    ));
                }
            }
            let superseded = holder.and_then(|holder| self.inner.detach_locked(&mut conns, holder));
            let user = user.map(Arc::<str>::from);
            if let Some(user) = &user {
                *conns.per_user.entry(Arc::clone(user)).or_insert(0) += 1;
            }
            let device = device
                .zip(device_topic.as_deref())
                .map(|(device, topic)| (Arc::<str>::from(device), Arc::<str>::from(topic)));
            if let Some((device, _)) = &device {
                conns.devices.insert(Arc::clone(device), id);
            }
            let conn = conns.by_id.get_mut(&id).ok_or_else(gone)?;
            conn.authenticated = true;
            conn.user = user;
            conn.device = device;
            let auto = Subscribe {
                explicit: false,
                max: usize::MAX,
                mark: None,
                ack: None,
            };
            for topic in [&user_topic, &device_topic].into_iter().flatten() {
                self.inner
                    .subscribe_locked(&mut conns, id, topic, &auto, &mut slow)?;
            }
            superseded
        };
        if let Some(conn) = superseded {
            signal(&conn.closer, CloseReason::Superseded);
        }
        self.inner.close_all(&slow, CloseReason::SlowConsumer);
        if let Some(topic) = device_topic {
            // Older holders of this device on other nodes step down.
            self.broadcast(topic, close_payload(CloseKind::Superseded));
        }
        Ok(())
    }

    /// Subscribes connection `id` to `topic`. Idempotent.
    pub(crate) fn subscribe(
        &self,
        id: ConnId,
        topic: &str,
        request: &Subscribe<'_>,
    ) -> Result<Cursor, Error> {
        let mut slow = Vec::new();
        let cursor = {
            let mut conns = lock(&self.inner.conns);
            self.inner
                .subscribe_locked(&mut conns, id, topic, request, &mut slow)
        };
        self.inner.close_all(&slow, CloseReason::SlowConsumer);
        cursor
    }

    /// Unsubscribes connection `id` from `topic`; `false` if it wasn't subscribed.
    pub(crate) fn unsubscribe(&self, id: ConnId, topic: &str) -> bool {
        let mut conns = lock(&self.inner.conns);
        let Some(conn) = conns.by_id.get_mut(&id) else {
            return false;
        };
        let Some(explicit) = conn.topics.remove(topic) else {
            return false;
        };
        conn.explicit -= usize::from(explicit);
        self.inner.drop_subscriber(topic, id);
        true
    }

    /// Runs client action `name` on the connection holding `user`'s `device`
    /// — on this node if it is here, else on whichever node holds it.
    pub(crate) async fn client_call_device(
        &self,
        user: &str,
        device: &str,
        name: &str,
        params: Value,
    ) -> Result<Value, Error> {
        let topic = DeviceChannel { user, id: device }.topic()?;
        if let Some(client) = self.inner.device_client(&topic) {
            return client.call(name, params).await;
        }
        let params = serde_json::to_string(&params).map_err(|e| Error::encode(&e))?;
        let payload = publish_payload(name, &params);
        self.inner.broker.validate(&topic, &payload)?;
        self.inner
            .broker
            .call(&topic, payload, self.inner.call_timeout)
            .await
    }

    /// Hands `payload` to the broker without waiting (best effort).
    fn broadcast(&self, topic: String, payload: Bytes) {
        let broker = Arc::clone(&self.inner.broker);
        let send = async move {
            let _ = broker.publish(&topic, payload).await;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn(send)),
            Err(_) => drop(send.now_or_never()),
        }
    }
}

fn already_authenticated() -> Error {
    Error::coded(409, "conflict", "session already authenticated")
}

/// Validates and encodes an identity's auto topics before anything mutates.
fn identity_topics(
    user: Option<&str>,
    device: Option<&str>,
) -> Result<(Option<String>, Option<String>), Error> {
    let user_topic = user.map(|id| UserChannel { id }.topic()).transpose()?;
    let device_topic = match (user, device) {
        (_, None) => None,
        (Some(user), Some(id)) => Some(DeviceChannel { user, id }.topic()?),
        (None, Some(_)) => {
            return Err(Error::coded(
                400,
                "bad_request",
                "a device identity needs a user",
            ));
        }
    };
    Ok((user_topic, device_topic))
}

impl BrokerSink {
    /// Delivers a payload another node published on `topic` to local
    /// subscribers (ignored without any — checked before parsing).
    pub fn deliver(&self, topic: &str, payload: &[u8]) {
        if let Some(hub) = self.hub.upgrade() {
            hub.deliver(topic, payload);
        }
    }

    /// The broker lost messages (e.g. its connection dropped): client-requested
    /// subscriptions are dropped for re-authorization, auto channels start a
    /// new epoch (see `docs/pubsub.md`).
    pub fn reset(&self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.reset();
        }
    }

    /// Runs a cross-node device call if this node holds the device `topic`;
    /// `None` otherwise (so the broker stays silent).
    pub async fn call(&self, topic: &str, payload: &[u8]) -> Option<Result<Value, Error>> {
        let client = self.hub.upgrade()?.device_client(topic)?;
        let Ok(incoming) = serde_json::from_slice::<Incoming<'_>>(payload) else {
            return Some(Err(Error::coded(
                400,
                "bad_request",
                "malformed broker call",
            )));
        };
        let name = incoming.name.unwrap_or_default();
        let params = incoming
            .params
            .map_or(Ok(Value::Null), |raw| serde_json::from_str(raw.get()));
        Some(match params {
            Ok(params) => client.call(&name, params).await,
            Err(error) => Err(Error::decode(&error)),
        })
    }
}
