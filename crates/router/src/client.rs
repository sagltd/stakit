//! Server→client actions (the duplex `cx.client_call` path).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::Error;

/// A client-side action the server may invoke over a duplex connection.
pub trait ClientAction {
    /// Parameters sent to the client.
    type Params: Serialize;
    /// Value returned by the client.
    type Return: DeserializeOwned;
    /// Stable name (matched on the client).
    const NAME: &'static str;
}

/// Handle used by [`Cx::client_call`](crate::Cx::client_call) to invoke client
/// actions. Disconnected on unary/stream transports (calls error).
#[derive(Clone, Default)]
pub(crate) struct ClientHandle(Option<Arc<Inner>>);

struct Inner {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>,
    next_id: AtomicU64,
}

impl ClientHandle {
    /// Builds a connected handle wired to a session's outgoing channel.
    pub(crate) fn connected(outgoing: mpsc::UnboundedSender<Value>) -> Self {
        Self(Some(Arc::new(Inner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        })))
    }

    /// Invokes a client action by name, awaiting the client's response.
    pub(crate) async fn call(&self, name: &str, params: Value) -> Result<Value, Error> {
        let Some(inner) = &self.0 else {
            return Err(Error::new(
                400,
                "client actions require a duplex connection",
            ));
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
            .map_err(|_| Error::new(500, "client connection closed"))?;
        rx.await
            .map_err(|_| Error::new(500, "client connection closed"))?
    }

    /// Resolves a pending client-action call with the client's reply.
    pub(crate) fn resolve(&self, id: u64, result: Result<Value, Error>) {
        if let Some(inner) = &self.0 {
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
