//! Server→client actions (the duplex `cx.client_call` path).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
    outgoing: mpsc::Sender<Value>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>,
    next_id: AtomicU64,
    timeout: Duration,
}

impl ClientHandle {
    /// Builds a connected handle wired to a session's outgoing channel, with the
    /// given per-`client_call` reply timeout.
    pub(crate) fn connected(outgoing: mpsc::Sender<Value>, timeout: Duration) -> Self {
        Self::Connected(Arc::new(Inner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            timeout,
        }))
    }

    /// Builds a mock handle that answers `client_call` via `handler` (tests).
    pub(crate) fn mock(handler: Arc<MockHandler>) -> Self {
        Self::Mock(handler)
    }

    /// Invokes a client action by name, awaiting the client's response.
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
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        inner
            .pending
            .lock()
            .expect("pending lock poisoned")
            .insert(id, tx);
        let frame = json!({ "kind": "client_call", "id": id, "name": name, "params": params });
        inner
            .outgoing
            .send(frame)
            .await
            .map_err(|_| Error::new(500, "client connection closed"))?;
        match tokio::time::timeout(inner.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::new(500, "client connection closed")),
            Err(_) => {
                // timed out — drop the pending entry so the task can't leak.
                inner
                    .pending
                    .lock()
                    .expect("pending lock poisoned")
                    .remove(&id);
                Err(Error::new(504, "client action timed out"))
            }
        }
    }

    /// Resolves a pending client-action call with the client's reply.
    pub(crate) fn resolve(&self, id: u64, result: Result<Value, Error>) {
        if let Self::Connected(inner) = self {
            let entry = inner
                .pending
                .lock()
                .expect("pending lock poisoned")
                .remove(&id);
            if let Some(tx) = entry {
                let _ = tx.send(result);
            }
        }
    }
}
