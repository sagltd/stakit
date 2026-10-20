//! `stakit-pubsub-postgres` — a Postgres `LISTEN`/`NOTIFY` [`Broker`] for
//! stakit-router pub/sub, so hubs on several nodes share topics without extra
//! infrastructure.
//!
//! - Every node `LISTEN`s on one channel (default `stakit_pubsub`) over one
//!   dedicated connection.
//! - Outgoing messages go through one publisher task that batches them into a
//!   single `select pg_notify(…) from unnest(…)` per round trip.
//! - Every envelope is HMAC-SHA256-signed with a secret shared by the nodes
//!   and timestamped; unsigned, forged, replayed or stale (> 30s) envelopes
//!   are dropped, so `NOTIFY` access alone can't inject traffic.
//! - A `NOTIFY` payload must stay below 8000 bytes: bigger publishes fail with
//!   `413` before anyone gets them — publish ids, fetch the data.
//! - When the listener connection drops it reconnects with backoff; local
//!   subscribers are reset on loss and again once listening resumes.
//!
//! ```ignore
//! let broker = PostgresBroker::connect(pool.clone(), &secret).await?;
//! let router = Router::builder().ctx(app).hub(Hub::new(broker)) /* … */ .build();
//! ```

mod wire;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::future::BoxFuture;
use hashbrown::HashMap;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use stakit_router::{Broker, BrokerSink, Error};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::AbortHandle;

use wire::{Body, Failure, Key, Opened};

/// Default `LISTEN`/`NOTIFY` channel.
pub const DEFAULT_CHANNEL: &str = "stakit_pubsub";

/// Outgoing messages queued for the publisher task.
const OUTBOX: usize = 4096;

/// Messages sent per `pg_notify` round trip.
const BATCH: usize = 256;

/// Cross-node device calls this node runs at once; extra ones are dropped
/// (the caller times out).
const MAX_INBOUND_CALLS: usize = 64;

/// Listener reconnect backoff.
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(30);

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>;

struct Inner {
    key: Key,
    node: u64,
    counter: AtomicU64,
    sink: OnceLock<BrokerSink>,
    outbox: mpsc::Sender<String>,
    pending: Pending,
    next_call: AtomicU64,
    /// Highest counter seen per origin node (replay guard).
    seen: Mutex<HashMap<u64, u64>>,
    calls: Arc<Semaphore>,
    listener: OnceLock<AbortHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.get() {
            listener.abort();
        }
    }
}

/// A [`Broker`] over Postgres `LISTEN`/`NOTIFY`. One per node.
pub struct PostgresBroker {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn random_node() -> Result<u64, Error> {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(Error::internal)?;
    Ok(u64::from_le_bytes(bytes))
}

impl PostgresBroker {
    /// Starts listening on [`DEFAULT_CHANNEL`] (requires a Tokio runtime).
    /// `secret` (≥ 32 random bytes, the same on every node) signs the bus.
    ///
    /// # Errors
    /// A short secret, or if the listener connection can't be opened.
    pub async fn connect(pool: PgPool, secret: &[u8]) -> Result<Self, Error> {
        Self::connect_on(pool, DEFAULT_CHANNEL, secret).await
    }

    /// Like [`PostgresBroker::connect`], on a custom channel (to keep apps
    /// sharing a database apart — not a security boundary; the secret is).
    ///
    /// # Errors
    /// A short secret, or if the listener connection can't be opened.
    pub async fn connect_on(pool: PgPool, channel: &str, secret: &[u8]) -> Result<Self, Error> {
        let key = Key::new(secret)?;
        let listener = listen_on(&pool, channel).await.map_err(Error::internal)?;
        let (outbox, queued) = mpsc::channel(OUTBOX);
        let inner = Arc::new(Inner {
            key,
            node: random_node()?,
            counter: AtomicU64::new(0),
            sink: OnceLock::new(),
            outbox,
            pending: Mutex::default(),
            next_call: AtomicU64::new(0),
            seen: Mutex::default(),
            calls: Arc::new(Semaphore::new(MAX_INBOUND_CALLS)),
            listener: OnceLock::new(),
        });
        tokio::spawn(publish_loop(pool.clone(), channel.to_owned(), queued));
        let task = listen_loop(Arc::downgrade(&inner), pool, channel.to_owned(), listener);
        let _ = inner.listener.set(tokio::spawn(task).abort_handle());
        Ok(Self { inner })
    }
}

async fn listen_on(pool: &PgPool, channel: &str) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(channel).await?;
    Ok(listener)
}

/// Drains the outbox: one `pg_notify` round trip per batch, in queue order.
/// Ends when the broker (the only sender) is dropped.
async fn publish_loop(pool: PgPool, channel: String, mut queued: mpsc::Receiver<String>) {
    let mut batch = Vec::with_capacity(BATCH);
    while queued.recv_many(&mut batch, BATCH).await > 0 {
        let sent = sqlx::query("select pg_notify($1, payload) from unnest($2::text[]) as payload")
            .bind(&channel)
            .bind(&batch)
            .execute(&pool)
            .await;
        if let Err(error) = sent {
            tracing::warn!(%error, dropped = batch.len(), "pub/sub NOTIFY failed");
        }
        batch.clear();
    }
}

/// Receives notifications until the broker is dropped. On connection loss:
/// reset local subscribers, reconnect with backoff, reset again.
async fn listen_loop(inner: Weak<Inner>, pool: PgPool, channel: String, first: PgListener) {
    let mut current = first;
    loop {
        loop {
            let received = current.try_recv().await;
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let Ok(Some(notification)) = received else {
                tracing::warn!("pub/sub listener lost its connection; reconnecting");
                inner.reset_once();
                break;
            };
            inner.handle(notification.payload());
        }
        current = reconnect(&pool, &channel).await;
        // Messages may have been missed while reconnecting.
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.reset_once();
    }
}

async fn reconnect(pool: &PgPool, channel: &str) -> PgListener {
    let mut delay = RETRY_MIN;
    loop {
        match listen_on(pool, channel).await {
            Ok(listener) => return listener,
            Err(error) => {
                tracing::warn!(%error, "pub/sub listener reconnect failed; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
    }
}

/// Removes a call's pending entry however the call ends.
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(self.pending).remove(&self.id);
    }
}

impl Inner {
    fn reset_once(&self) {
        if let Some(sink) = self.sink.get() {
            sink.reset();
        }
    }

    /// Signs and queues one message. A full queue is a `503`.
    fn send(&self, body: &Body<'_>) -> Result<(), Error> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        let text = wire::seal(&self.key, self.node, counter, now_ms(), body)?;
        self.outbox.try_send(text).map_err(|error| match error {
            TrySendError::Full(_) => Error::coded(503, "unavailable", "pub/sub backlog full"),
            TrySendError::Closed(_) => Error::coded(503, "unavailable", "pub/sub broker stopped"),
        })
    }

    /// Drops envelopes already seen from `origin` (replays).
    fn fresh(&self, origin: u64, counter: u64) -> bool {
        let mut seen = lock(&self.seen);
        let last = seen.entry(origin).or_insert(0);
        let fresh = counter >= *last;
        if fresh {
            *last = counter + 1;
        }
        drop(seen);
        fresh
    }

    fn handle(self: &Arc<Self>, payload: &str) {
        let Some(Opened {
            origin,
            counter,
            body,
        }) = wire::open(&self.key, payload, now_ms())
        else {
            return;
        };
        if origin == self.node || !self.fresh(origin, counter) {
            return;
        }
        let Some(sink) = self.sink.get() else {
            return;
        };
        match body {
            Body::Publish { topic, p } => sink.deliver(&topic, p.get().as_bytes()),
            Body::Call { id, topic, p } => {
                let Ok(permit) = Arc::clone(&self.calls).try_acquire_owned() else {
                    return;
                };
                let (inner, sink) = (Arc::clone(self), sink.clone());
                let (topic, request) = (topic.into_owned(), p.get().to_owned());
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Some(result) = sink.call(&topic, request.as_bytes()).await {
                        inner.reply(origin, id, result);
                    }
                });
            }
            Body::Reply { to, id, ok, err } if to == self.node => {
                let result = err.map_or_else(
                    || Ok(ok.unwrap_or(Value::Null)),
                    |failure| Err(Error::new(failure.code, failure.message)),
                );
                let waiter = lock(&self.pending).remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(result);
                }
            }
            Body::Reply { .. } => {}
        }
    }

    fn reply(&self, to: u64, id: u64, result: Result<Value, Error>) {
        let body = |result: Result<Value, Error>| {
            let (ok, err) = match result {
                Ok(value) => (Some(value), None),
                Err(error) => (
                    None,
                    Some(Failure {
                        code: error.code,
                        message: error.message,
                    }),
                ),
            };
            Body::Reply { to, id, ok, err }
        };
        // An answer too big for NOTIFY still gets an (error) answer.
        if let Err(error) = self.send(&body(result)) {
            let _ = self.send(&body(Err(error)));
        }
    }
}

impl Broker for PostgresBroker {
    fn attach(&self, sink: BrokerSink) {
        let _ = self.inner.sink.set(sink);
    }

    // Every node receives every notification; the hub filters by topic.
    fn subscribe(&self, _topic: &str) {}

    fn unsubscribe(&self, _topic: &str) {}

    fn validate(&self, topic: &str, payload: &[u8]) -> Result<(), Error> {
        wire::check_len(wire::publish_len(topic, payload))
    }

    fn publish<'a>(&'a self, topic: &'a str, payload: Bytes) -> BoxFuture<'a, Result<(), Error>> {
        let sent = wire::raw(&payload).and_then(|p| {
            self.inner.send(&Body::Publish {
                topic: topic.into(),
                p,
            })
        });
        Box::pin(async move { sent })
    }

    fn call<'a>(
        &'a self,
        topic: &'a str,
        payload: Bytes,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            let inner = &self.inner;
            let id = inner.next_call.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            lock(&inner.pending).insert(id, tx);
            let _guard = PendingGuard {
                pending: &inner.pending,
                id,
            };
            let p = wire::raw(&payload)?;
            inner.send(&Body::Call {
                id,
                topic: topic.into(),
                p,
            })?;
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(Error::internal("pub/sub broker dropped the call")),
                Err(_) => Err(Error::coded(504, "timeout", "client action timed out")),
            }
        })
    }
}
