//! Duplex (websocket) session: calls, server→client `client_call`, pub/sub.
//!
//! Transport-agnostic. Pump inbound frames in via [`Session::handle_bytes`],
//! forward [`Session::outgoing`] (pre-serialized JSON text) to your socket, and
//! close the socket when [`Session::closed`] resolves. Requires a Tokio runtime
//! (actions run as spawned tasks so a `client_call` can suspend awaiting a reply
//! that arrives on a later inbound frame; a timer task drives heartbeats).

mod config;
mod inbound;
mod lifecycle;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::AbortHandle;

pub(crate) use config::encode;
pub use config::{CloseReason, Identity, SessionConfig};

use crate::client::ClientHandle;
use crate::pubsub::hub::{Closer, ConnId, Subscribe};
use crate::reply::ErrorBody;
use crate::{Cx, Error, Router};

/// Max concurrently-running tasks (calls, auth, subscribes) per connection. A
/// hostile peer can't spawn unbounded tasks by flooding frames — excess ones
/// are rejected (`429`) immediately rather than queued, so a suspended
/// `client_call` can't deadlock the resume path.
const MAX_INFLIGHT_CALLS: usize = 512;

/// A live duplex session over one connection.
pub struct Session<G, R> {
    shared: Arc<Shared<G, R>>,
    out_rx: Option<mpsc::Receiver<Bytes>>,
    lifecycle: AbortHandle,
}

pub(crate) struct Shared<G, R> {
    router: Arc<Router<G, R>>,
    req: Mutex<R>,
    conn: ConnId,
    client: ClientHandle,
    out_tx: mpsc::Sender<Bytes>,
    inflight: Arc<Semaphore>,
    closer: Arc<Closer>,
    authenticated: AtomicBool,
    /// One `auth` frame at a time.
    authenticating: AtomicBool,
    born: Instant,
    /// Milliseconds after `born` of the last inbound frame.
    last_seen: AtomicU64,
    /// Inbound frame rate window: `second << 32 | frames in that second`.
    rate: AtomicU64,
}

impl<G, R> Shared<G, R>
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    fn req(&self) -> R {
        self.req
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn cx(&self) -> Cx<G, R> {
        Cx {
            app: Arc::clone(&self.router.app),
            req: self.req(),
            client: self.client.clone(),
            hub: self.router.hub.clone(),
        }
    }

    fn authenticate(&self, req: R, identity: &Identity) -> Result<(), Error> {
        let limit = self.router.session_config.max_connections_per_user;
        self.router.hub.authenticate(self.conn, identity, limit)?;
        // Request context first: calls only see the flag once it is in place.
        *self.req.lock().unwrap_or_else(PoisonError::into_inner) = req;
        self.authenticated.store(true, Ordering::Release);
        Ok(())
    }
}

impl<G, R> Shared<G, R> {
    fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::Acquire)
    }

    fn is_closed(&self) -> bool {
        self.closer.borrow().is_some()
    }

    fn close(&self, reason: CloseReason) {
        self.router.hub.close(self.conn, reason);
        // Already detached (e.g. kicked as slow)? Still record the first reason.
        crate::pubsub::hub::signal(&self.closer, reason);
        self.client.fail_pending();
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Records an inbound frame; `false` once the per-second budget is spent.
    fn touch(&self) -> bool {
        let now = self.elapsed_ms();
        self.last_seen.store(now, Ordering::Relaxed);
        let second = now / 1000;
        let limit = u64::from(self.router.session_config.max_frames_per_second);
        let window = self.rate.fetch_add(1, Ordering::AcqRel);
        if window >> 32 != second {
            self.rate.store((second << 32) | 1, Ordering::Release);
            return true;
        }
        (window & u64::from(u32::MAX)) < limit
    }

    fn idle_for(&self) -> Duration {
        let seen = Duration::from_millis(self.last_seen.load(Ordering::Relaxed));
        self.born.elapsed().saturating_sub(seen)
    }

    /// Queues a frame without waiting; a full queue closes the session as a
    /// slow consumer (the reply must not be silently lost).
    fn send_now(&self, frame: Bytes) {
        if let Err(TrySendError::Full(_)) = self.out_tx.try_send(frame) {
            self.close(CloseReason::SlowConsumer);
        }
    }

    /// Queues a reply to request `id` (errors pass through `on_error`).
    fn reply_now(&self, id: u64, result: Result<Value, Error>) {
        let result = result.map_err(|error| (self.router.on_error)(error));
        self.send_now(result_frame(id, result));
    }
}

impl<G, R> Session<G, R>
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(router: Arc<Router<G, R>>, req: R) -> Self {
        let config = &router.session_config;
        let (out_tx, out_rx) = mpsc::channel(config.outgoing_buffer.max(1));
        let client = ClientHandle::connected(out_tx.clone(), router.client_call_timeout);
        let closer = Arc::new(watch::Sender::new(None));
        let conn = router
            .hub
            .register(out_tx.clone(), client.clone(), Arc::clone(&closer));
        let auth_window = router.auth.is_some().then_some(config.auth_window);
        let timers = lifecycle::Timers {
            heartbeat: config.heartbeat,
            idle_timeout: config.idle_timeout,
            auth_window,
        };
        let shared = Arc::new(Shared {
            router,
            req: Mutex::new(req),
            conn,
            client,
            out_tx,
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT_CALLS)),
            closer,
            authenticated: AtomicBool::new(auth_window.is_none()),
            authenticating: AtomicBool::new(false),
            born: Instant::now(),
            last_seen: AtomicU64::new(0),
            rate: AtomicU64::new(0),
        });
        let lifecycle = tokio::spawn(timers.run(Arc::downgrade(&shared), conn)).abort_handle();
        Self {
            shared,
            out_rx: Some(out_rx),
            lifecycle,
        }
    }

    /// Takes the outbound frame receiver — each item is one JSON text frame;
    /// forward it to your socket as-is. `Some` the first time only.
    pub const fn outgoing(&mut self) -> Option<mpsc::Receiver<Bytes>> {
        self.out_rx.take()
    }

    /// Resolves with the reason once the server ends this session (auth
    /// timeout, idle, slow consumer, …). Flush what's left in
    /// [`Session::outgoing`], then close the socket with
    /// [`CloseReason::code`].
    pub async fn closed(&self) -> CloseReason {
        let mut watcher = self.shared.closer.subscribe();
        let reason = watcher
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|seen| *seen);
        reason.unwrap_or(CloseReason::Idle)
    }

    /// This session's connection id on its hub.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.shared.conn
    }

    /// Authenticates the session from the adapter (e.g. a token in the upgrade
    /// request's headers) instead of an `auth` frame: replaces the request
    /// context and subscribes the identity's `user:` / `device:` channels.
    /// All or nothing — on error nothing changed.
    ///
    /// # Errors
    /// `400` for an invalid identity, `429` past the per-user connection cap,
    /// `409` if already authenticated or the device is held by another user.
    pub fn authenticate(&self, req: R, identity: &Identity) -> Result<(), Error> {
        self.shared.authenticate(req, identity)
    }

    /// Subscribes this session to `topic` server-side (trusted: no
    /// authorization, no subscription cap, kept across broker resets).
    /// Returns `(epoch, seq)`.
    ///
    /// # Errors
    /// `410` if the session is closed.
    pub fn subscribe(&self, topic: &str) -> Result<(u64, u64), Error> {
        let request = Subscribe {
            explicit: false,
            max: usize::MAX,
            mark: None,
            ack: None,
        };
        let cursor = self
            .shared
            .router
            .hub
            .subscribe(self.shared.conn, topic, &request)?;
        Ok((cursor.epoch, cursor.seq))
    }

    /// Unsubscribes this session from `topic`; `false` if it wasn't subscribed.
    pub fn unsubscribe(&self, topic: &str) -> bool {
        self.shared.router.hub.unsubscribe(self.shared.conn, topic)
    }

    /// Handles one raw inbound text frame, enforcing the max frame size (an
    /// oversized frame closes the session). Prefer this over [`Session::handle`].
    pub fn handle_bytes(&self, bytes: &[u8]) {
        if bytes.len() > self.shared.router.session_config.max_frame_bytes {
            self.shared.close(CloseReason::FrameTooLarge);
            return;
        }
        if let Ok(frame) = serde_json::from_slice::<Value>(bytes) {
            self.handle(&frame);
        }
    }

    /// Handles one parsed inbound frame (see `docs/transport.md`):
    /// `call`, `client_result`, `auth`, `subscribe`, `unsubscribe`, `pong`.
    /// Until the session is authenticated (when an auth hook is set), `call` /
    /// `subscribe` / `unsubscribe` are answered with `401`. More than
    /// [`SessionConfig::max_frames_per_second`] frames close the session.
    pub fn handle(&self, frame: &Value) {
        let shared = &self.shared;
        if shared.is_closed() {
            return;
        }
        if !shared.touch() {
            shared.close(CloseReason::RateLimited);
            return;
        }
        let kind = frame.get("kind").and_then(Value::as_str);
        let id = frame.get("id").and_then(Value::as_u64);
        match (kind, id) {
            (Some("client_result"), Some(id)) => self.resolve_client(id, frame),
            (Some("auth"), Some(id)) => self.spawn_auth(id, frame),
            (Some("call" | "subscribe" | "unsubscribe"), Some(id))
                if !shared.is_authenticated() =>
            {
                shared.reply_now(
                    id,
                    Err(Error::coded(401, "unauthorized", "authenticate first")),
                );
            }
            (Some("call"), Some(id)) => self.spawn_call(id, frame),
            (Some("subscribe"), Some(id)) => self.spawn_subscribe(id, frame),
            (Some("unsubscribe"), Some(id)) => {
                let topic = frame
                    .get("topic")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                shared.reply_now(id, Ok(Value::Bool(self.unsubscribe(topic))));
            }
            _ => {}
        }
    }
}

impl<G, R> Drop for Session<G, R> {
    fn drop(&mut self) {
        self.lifecycle.abort();
        self.shared.router.hub.remove(self.shared.conn);
        self.shared.client.fail_pending();
    }
}

fn result_frame(id: u64, result: Result<Value, Error>) -> Bytes {
    encode(&match result {
        Ok(data) => json!({ "kind": "result", "id": id, "status": "ok", "data": data }),
        Err(error) => {
            let body = serde_json::to_value(ErrorBody::from(error)).unwrap_or(Value::Null);
            json!({ "kind": "result", "id": id, "status": "error", "error": body })
        }
    })
}
