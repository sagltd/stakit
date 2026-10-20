//! The hub's shared state and every operation that touches its locks.
//!
//! Locking: `conns` (a `Mutex`) is always taken before a topic shard (an
//! `RwLock`), and a shard before a topic's cursor `Mutex` — never the other way
//! round — and no lock is held across `.await`. A publish holds one shard's
//! read lock plus that topic's cursor lock while it `try_send`s the
//! pre-serialized frame to every subscriber, so a topic's `seq` order is its
//! send order.
#![expect(
    clippy::significant_drop_tightening,
    reason = "locks are held on purpose for the whole step (see the module docs)"
)]

use std::hash::BuildHasher as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use bytes::Bytes;
use hashbrown::{DefaultHashBuilder, HashMap};
use tokio::sync::{mpsc, watch};

use super::frames::{CloseKind, Incoming, publish_frame, reset_frame, unsubscribed_frame};
use crate::Error;
use crate::client::ClientHandle;
use crate::pubsub::broker::Broker;
use crate::session::CloseReason;

/// Topic index shards. A long fan-out only blocks writers of its own shard.
const SHARDS: usize = 64;

/// Revocation counter slots (topics hash into them; a collision only costs a
/// spurious re-authorize).
const REVOCATION_SLOTS: usize = 256;

/// Epochs and seqs stay below 2^53 so JavaScript numbers carry them exactly.
pub(crate) const JS_SAFE: u64 = (1 << 53) - 1;

/// Connection id, unique per hub.
pub(crate) type ConnId = u64;

/// Signals a session's close reason to its adapter.
pub(crate) type Closer = watch::Sender<Option<CloseReason>>;

/// Where a topic's stream stands: `epoch` changes when its history resets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub(crate) epoch: u64,
    pub(crate) seq: u64,
}

pub(crate) struct Topic {
    pub(crate) subscribers: HashMap<ConnId, mpsc::Sender<Bytes>>,
    cursor: Mutex<Cursor>,
}

pub(crate) struct Conn {
    pub(crate) tx: mpsc::Sender<Bytes>,
    pub(crate) client: ClientHandle,
    pub(crate) closer: Arc<Closer>,
    pub(crate) authenticated: bool,
    pub(crate) user: Option<Arc<str>>,
    /// `(device id, device topic)` this connection holds exclusively.
    pub(crate) device: Option<(Arc<str>, Arc<str>)>,
    /// Subscribed topics → whether the client asked for it (vs auto channel).
    pub(crate) topics: HashMap<Arc<str>, bool>,
    pub(crate) explicit: usize,
}

#[derive(Default)]
pub(crate) struct Conns {
    pub(crate) by_id: HashMap<ConnId, Conn>,
    pub(crate) per_user: HashMap<Arc<str>, usize>,
    /// Device id → its local holder.
    pub(crate) devices: HashMap<Arc<str>, ConnId>,
}

/// How a subscribe was requested.
pub(crate) struct Subscribe<'a> {
    /// The client asked (counts toward `max`, dropped on broker reset).
    pub(crate) explicit: bool,
    pub(crate) max: usize,
    /// Revocation mark read before `authorize`; refused if it moved since.
    pub(crate) mark: Option<u64>,
    /// Builds the ack frame, queued under the shard lock so no publish can
    /// overtake it.
    pub(crate) ack: Option<&'a dyn Fn(Cursor) -> Bytes>,
}

type Shard = RwLock<HashMap<Arc<str>, Topic>>;

pub(crate) struct Inner {
    shards: Box<[Shard]>,
    revocations: Box<[AtomicU64]>,
    hasher: DefaultHashBuilder,
    pub(crate) conns: Mutex<Conns>,
    pub(crate) next_conn: AtomicU64,
    epoch_base: u64,
    next_epoch: AtomicU64,
    pub(crate) broker: Arc<dyn Broker>,
    pub(crate) call_timeout: Duration,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn gone() -> Error {
    Error::coded(410, "gone", "connection closed")
}

impl Inner {
    pub(crate) fn new(broker: Arc<dyn Broker>, call_timeout: Duration, epoch_base: u64) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
            revocations: (0..REVOCATION_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            hasher: DefaultHashBuilder::default(),
            conns: Mutex::default(),
            next_conn: AtomicU64::new(1),
            epoch_base,
            next_epoch: AtomicU64::new(0),
            broker,
            call_timeout,
        }
    }

    fn slot(&self, topic: &str, slots: usize) -> usize {
        let hash = self.hasher.hash_one(topic);
        // `slots` is tiny, so the remainder always fits a `usize`.
        usize::try_from(hash % u64::try_from(slots).unwrap_or(1)).unwrap_or(0)
    }

    fn shard(&self, topic: &str) -> &Shard {
        let index = self.slot(topic, SHARDS);
        self.shards.get(index).unwrap_or(&self.shards[0])
    }

    pub(crate) fn read_shard(&self, topic: &str) -> RwLockReadGuard<'_, HashMap<Arc<str>, Topic>> {
        self.shard(topic)
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write_shard(&self, topic: &str) -> RwLockWriteGuard<'_, HashMap<Arc<str>, Topic>> {
        self.shard(topic)
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn revocation(&self, topic: &str) -> Option<&AtomicU64> {
        self.revocations.get(self.slot(topic, REVOCATION_SLOTS))
    }

    pub(crate) fn revocation_mark(&self, topic: &str) -> u64 {
        self.revocation(topic)
            .map_or(0, |slot| slot.load(Ordering::Acquire))
    }

    fn revoke(&self, topic: &str) {
        if let Some(slot) = self.revocation(topic) {
            slot.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn new_epoch(&self) -> u64 {
        let step = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        self.epoch_base.wrapping_add(step) & JS_SAFE
    }

    pub(crate) fn has_topic(&self, topic: &str) -> bool {
        self.read_shard(topic).contains_key(topic)
    }

    /// Subscribes `id` to `topic` with `conns` already locked.
    pub(crate) fn subscribe_locked(
        &self,
        conns: &mut Conns,
        id: ConnId,
        topic: &str,
        request: &Subscribe<'_>,
        slow: &mut Vec<ConnId>,
    ) -> Result<Cursor, Error> {
        let conn = conns.by_id.get_mut(&id).ok_or_else(gone)?;
        if let Some(mark) = request.mark
            && mark != self.revocation_mark(topic)
        {
            return Err(Error::coded(
                409,
                "subscription_revoked",
                "access changed while subscribing; retry",
            ));
        }
        let mut shard = self.write_shard(topic);
        let already = conn.topics.contains_key(topic);
        if !already && request.explicit && conn.explicit >= request.max {
            return Err(Error::coded(
                429,
                "too_many_subscriptions",
                "subscription limit reached",
            ));
        }
        if !shard.contains_key(topic) {
            self.broker.subscribe(topic);
            let cursor = Cursor {
                epoch: self.new_epoch(),
                seq: 0,
            };
            let state = Topic {
                subscribers: HashMap::new(),
                cursor: Mutex::new(cursor),
            };
            shard.insert(Arc::from(topic), state);
        }
        let (key, state) = shard.get_key_value_mut(topic).ok_or_else(gone)?;
        if !already {
            state.subscribers.insert(id, conn.tx.clone());
            conn.topics.insert(Arc::clone(key), request.explicit);
            conn.explicit += usize::from(request.explicit);
        }
        let cursor = *lock(&state.cursor);
        if let Some(ack) = request.ack
            && conn.tx.try_send(ack(cursor)).is_err()
        {
            slow.push(id);
        }
        Ok(cursor)
    }

    /// Fans a publish out to `topic`'s local subscribers.
    pub(crate) fn fan_out(&self, topic: &str, name: &str, params: &str) {
        let mut slow = Vec::new();
        {
            let shard = self.read_shard(topic);
            let Some(state) = shard.get(topic) else {
                return;
            };
            let mut cursor = lock(&state.cursor);
            cursor.seq = (cursor.seq + 1) & JS_SAFE;
            let frame = publish_frame(topic, name, params, cursor.epoch, cursor.seq);
            for (id, tx) in &state.subscribers {
                if tx.try_send(frame.clone()).is_err() {
                    slow.push(*id);
                }
            }
        }
        self.close_all(&slow, CloseReason::SlowConsumer);
    }

    /// Delivers a payload from another node. Looks the topic up before parsing.
    pub(crate) fn deliver(&self, topic: &str, payload: &[u8]) {
        if !self.has_topic(topic) {
            return;
        }
        let Ok(incoming) = serde_json::from_slice::<Incoming<'_>>(payload) else {
            return;
        };
        if let Some(user) = incoming.kick {
            self.kick_local(topic, &user);
        } else if let Some(kind) = incoming.close {
            self.close_topic(topic, kind);
        } else if let (Some(name), Some(params)) = (incoming.name, incoming.params) {
            self.fan_out(topic, &name, params.get());
        }
    }

    /// The broker lost messages: kicks may have been missed, so every
    /// client-requested subscription is dropped (`unsubscribed`, reason
    /// `reset`) for the client to resubscribe through `authorize` again; auto
    /// channels start a new epoch (`reset` frame).
    pub(crate) fn reset(&self) {
        for slot in &self.revocations {
            slot.fetch_add(1, Ordering::AcqRel);
        }
        let mut slow = Vec::new();
        {
            let mut conns = lock(&self.conns);
            for shard in &self.shards {
                let mut shard = shard.write().unwrap_or_else(PoisonError::into_inner);
                shard.retain(|topic, state| {
                    let epoch = self.new_epoch();
                    *lock(&state.cursor) = Cursor { epoch, seq: 0 };
                    let reset = reset_frame(topic, epoch);
                    let dropped = unsubscribed_frame(topic, "reset");
                    state.subscribers.retain(|id, tx| {
                        let conn = conns.by_id.get_mut(id);
                        let explicit = conn.as_ref().is_some_and(|conn| {
                            conn.topics.get(&**topic).copied().unwrap_or(false)
                        });
                        if explicit && let Some(conn) = conn {
                            conn.topics.remove(&**topic);
                            conn.explicit -= 1;
                        }
                        let frame = if explicit {
                            dropped.clone()
                        } else {
                            reset.clone()
                        };
                        if tx.try_send(frame).is_err() {
                            slow.push(*id);
                        }
                        !explicit
                    });
                    let alive = !state.subscribers.is_empty();
                    if !alive {
                        self.broker.unsubscribe(topic);
                    }
                    alive
                });
            }
        }
        self.close_all(&slow, CloseReason::SlowConsumer);
    }

    fn subscriber_ids(&self, topic: &str) -> Vec<ConnId> {
        self.read_shard(topic)
            .get(topic)
            .map_or_else(Vec::new, |state| {
                state.subscribers.keys().copied().collect()
            })
    }

    /// Force-unsubscribes `user`'s local connections from `topic`.
    pub(crate) fn kick_local(&self, topic: &str, user: &str) {
        let mut slow = Vec::new();
        {
            let mut conns = lock(&self.conns);
            self.revoke(topic);
            let ids = self.subscriber_ids(topic);
            let frame = unsubscribed_frame(topic, "kicked");
            for id in ids {
                let Some(conn) = conns.by_id.get_mut(&id) else {
                    continue;
                };
                if conn.user.as_deref() != Some(user) {
                    continue;
                }
                if let Some(explicit) = conn.topics.remove(topic) {
                    conn.explicit -= usize::from(explicit);
                }
                if conn.tx.try_send(frame.clone()).is_err() {
                    slow.push(id);
                }
                self.drop_subscriber(topic, id);
            }
        }
        self.close_all(&slow, CloseReason::SlowConsumer);
    }

    /// Closes every local subscriber of `topic` (device superseded / revoked).
    pub(crate) fn close_topic(&self, topic: &str, kind: CloseKind) {
        let ids = self.subscriber_ids(topic);
        let reason = match kind {
            CloseKind::Superseded => CloseReason::Superseded,
            CloseKind::Revoked => CloseReason::Revoked,
        };
        self.close_all(&ids, reason);
    }

    /// The local holder of a device topic (exclusive), if any.
    pub(crate) fn device_client(&self, topic: &str) -> Option<ClientHandle> {
        let conns = lock(&self.conns);
        let shard = self.read_shard(topic);
        let state = shard.get(topic)?;
        state.subscribers.keys().find_map(|id| {
            let conn = conns.by_id.get(id)?;
            let holds = conn
                .device
                .as_ref()
                .is_some_and(|(_, held)| &**held == topic);
            holds.then(|| conn.client.clone())
        })
    }

    pub(crate) fn close_all(&self, ids: &[ConnId], reason: CloseReason) {
        for id in ids {
            self.close(*id, reason);
        }
    }

    pub(crate) fn close(&self, id: ConnId, reason: CloseReason) {
        let conn = self.detach(id);
        if let Some(conn) = conn {
            signal(&conn.closer, reason);
        }
    }

    pub(crate) fn detach(&self, id: ConnId) -> Option<Conn> {
        let mut conns = lock(&self.conns);
        self.detach_locked(&mut conns, id)
    }

    /// Removes a connection and all its subscriptions; fails its pending
    /// client calls. Returns it so the caller can signal its closer.
    pub(crate) fn detach_locked(&self, conns: &mut Conns, id: ConnId) -> Option<Conn> {
        let conn = conns.by_id.remove(&id)?;
        if let Some(user) = &conn.user
            && let Some(count) = conns.per_user.get_mut(user)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                conns.per_user.remove(user);
            }
        }
        if let Some((device, _)) = &conn.device
            && conns.devices.get(device) == Some(&id)
        {
            conns.devices.remove(device);
        }
        for topic in conn.topics.keys() {
            self.drop_subscriber(topic, id);
        }
        conn.client.fail_pending();
        Some(conn)
    }

    /// Removes one subscriber; drops the topic (and broker interest) when empty.
    pub(crate) fn drop_subscriber(&self, topic: &str, id: ConnId) {
        let mut shard = self.write_shard(topic);
        let Some(state) = shard.get_mut(topic) else {
            return;
        };
        state.subscribers.remove(&id);
        if state.subscribers.is_empty() {
            shard.remove(topic);
            self.broker.unsubscribe(topic);
        }
    }
}

/// Records the first close reason.
pub(crate) fn signal(closer: &Closer, reason: CloseReason) {
    closer.send_if_modified(|current| current.is_none() && current.replace(reason).is_none());
}
