//! Server→client actions (the duplex `cx.client_call` path).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use hashbrown::HashMap;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use stakit_model::TSType;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::Error;

/// Default `client_call` reply timeout (30s).
///
/// Bounds memory: a disconnected or slow client can't leak the suspended action
/// forever. Override per-router with
/// [`Builder::client_call_timeout`](crate::Builder::client_call_timeout).
pub const DEFAULT_CLIENT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A client-side action the server may invoke over a duplex connection.
pub trait ClientAction {
    /// Parameters sent to the client.
    type Params: TSType + Serialize;
    /// Value returned by the client.
    type Return: TSType + DeserializeOwned;
    /// Stable name (matched on the client).
    const NAME: &'static str;
}

/// TypeScript metadata for a registered client action.
pub(crate) struct ClientMeta {
    pub(crate) name: &'static str,
    pub(crate) params_ref: String,
    pub(crate) return_ref: String,
    pub(crate) decls: std::collections::BTreeMap<String, String>,
}

/// A test stub for `client_call`: maps `(action name, params)` to a reply.
pub(crate) type MockHandler = dyn Fn(&str, Value) -> Result<Value, Error> + Send + Sync;

/// Handle backing [`Cx::client_call`](crate::Cx::client_call).
///
/// Three modes: `Disconnected` (unary/stream — calls error, no back-channel),
/// `Connected` (a live duplex session), and `Mock` (an in-process test stub, see
/// [`Cx::with_client`](crate::Cx::with_client)).
#[derive(Clone, Default)]
pub(crate) enum ClientHandle {
    /// No duplex connection (unary / HTTP stream).
    #[default]
    Disconnected,
    /// A live websocket session.
    Connected(Arc<Inner>),
    /// A test stub.
    Mock(Arc<MockHandler>),
}

pub(crate) struct Inner {
    outgoing: mpsc::Sender<Bytes>,
    pending: Mutex<HashMap<u64, Reply>>,
    next_id: AtomicU64,
    timeout: Duration,
    /// Set once the session closed: pending calls failed, new ones refused.
    closed: AtomicBool,
}

type Reply = oneshot::Sender<Result<Value, Error>>;

fn closed_error() -> Error {
    Error::coded(410, "gone", "client connection closed")
}

impl Inner {
    fn pending(&self) -> MutexGuard<'_, HashMap<u64, Reply>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Removes a call's pending entry however the call ends — reply, timeout, send
/// failure, or the caller's future being dropped.
struct PendingGuard<'a> {
    inner: &'a Inner,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.inner.pending().remove(&self.id);
    }
}

impl ClientHandle {
    /// Builds a connected handle wired to a session's outgoing channel, with the
    /// given per-`client_call` reply timeout.
    pub(crate) fn connected(outgoing: mpsc::Sender<Bytes>, timeout: Duration) -> Self {
        Self::Connected(Arc::new(Inner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            timeout,
            closed: AtomicBool::new(false),
        }))
    }

    /// Builds a mock handle that answers `client_call` via `handler` (tests).
    pub(crate) fn mock(handler: Arc<MockHandler>) -> Self {
        Self::Mock(handler)
    }

    /// Invokes a client action by name, awaiting the client's response. The
    /// whole round trip (queueing the frame included) is bounded by the timeout.
    pub(crate) async fn call(&self, name: &str, params: Value) -> Result<Value, Error> {
        let inner = match self {
            Self::Disconnected => {
                return Err(Error::new(
                    400,
                    "client actions require a duplex connection",
                ));
            }
            Self::Mock(handler) => return handler(name, params),
            Self::Connected(inner) => inner,
        };
        if inner.closed.load(Ordering::Acquire) {
            return Err(closed_error());
        }
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        inner.pending().insert(id, tx);
        let _guard = PendingGuard { inner, id };
        let frame = crate::session::encode(
            &json!({ "kind": "client_call", "id": id, "name": name, "params": params }),
        );
        let round_trip = async {
            inner
                .outgoing
                .send(frame)
                .await
                .map_err(|_| closed_error())?;
            rx.await.unwrap_or_else(|_| Err(closed_error()))
        };
        tokio::time::timeout(inner.timeout, round_trip)
            .await
            .unwrap_or_else(|_| Err(Error::coded(504, "timeout", "client action timed out")))
    }

    /// Resolves a pending client-action call with the client's reply.
    pub(crate) fn resolve(&self, id: u64, result: Result<Value, Error>) {
        if let Self::Connected(inner) = self {
            let entry = inner.pending().remove(&id);
            if let Some(tx) = entry {
                let _ = tx.send(result);
            }
        }
    }

    /// The session closed: fail every pending call now (instead of letting
    /// them wait out the timeout) and refuse new ones.
    pub(crate) fn fail_pending(&self) {
        if let Self::Connected(inner) = self {
            inner.closed.store(true, Ordering::Release);
            let pending: Vec<Reply> = inner.pending().drain().map(|(_, tx)| tx).collect();
            for tx in pending {
                let _ = tx.send(Err(closed_error()));
            }
        }
    }

    /// Calls waiting for a reply (tests).
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        match self {
            Self::Connected(inner) => inner.pending().len(),
            Self::Disconnected | Self::Mock(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(timeout: Duration) -> (ClientHandle, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(4);
        (ClientHandle::connected(tx, timeout), rx)
    }

    #[tokio::test]
    async fn cancelled_call_leaves_no_pending_entry() {
        let (client, _rx) = handle(Duration::from_secs(30));
        let call = client.call("x", Value::Null);
        let _ = tokio::time::timeout(Duration::from_millis(10), call).await;
        assert_eq!(client.pending_len(), 0);
    }

    #[tokio::test]
    async fn timed_out_call_is_504_and_cleaned_up() {
        let (client, _rx) = handle(Duration::from_millis(10));
        let error = client.call("x", Value::Null).await.unwrap_err();
        assert_eq!(error.code, 504);
        assert_eq!(client.pending_len(), 0);
    }

    #[tokio::test]
    async fn send_on_a_dropped_socket_fails_and_cleans_up() {
        let (client, rx) = handle(Duration::from_secs(30));
        drop(rx);
        let error = client.call("x", Value::Null).await.unwrap_err();
        assert_eq!(error.code, 410);
        assert_eq!(client.pending_len(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_still_times_out() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Bytes::new()).unwrap();
        let client = ClientHandle::connected(tx, Duration::from_millis(10));
        assert_eq!(client.call("x", Value::Null).await.unwrap_err().code, 504);
    }

    #[tokio::test]
    async fn closing_fails_pending_calls_and_refuses_new_ones() {
        let (client, _rx) = handle(Duration::from_secs(30));
        let waiting = tokio::spawn({
            let client = client.clone();
            async move { client.call("x", Value::Null).await }
        });
        while client.pending_len() == 0 {
            tokio::task::yield_now().await;
        }
        client.fail_pending();
        assert_eq!(waiting.await.unwrap().unwrap_err().code, 410);
        assert_eq!(client.call("x", Value::Null).await.unwrap_err().code, 410);
    }
}
