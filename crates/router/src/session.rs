//! Duplex (websocket) session: bidirectional, with server→client `client_call`.
//!
//! Transport-agnostic. You pump inbound frames in via [`Session::handle`] and
//! forward outbound frames (from [`Session::outgoing`]) to your socket. Requires
//! a Tokio runtime (actions run as spawned tasks so a `client_call` can suspend
//! awaiting a reply that arrives on a later inbound frame).

use std::sync::Arc;

use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use crate::client::ClientHandle;
use crate::reply::ErrorBody;
use crate::{Cx, Error, Router};

/// Outbound-frame buffer per connection. Bounds memory and applies backpressure
/// to action execution when a client drains its socket slowly.
const OUTGOING_BUFFER: usize = 1024;

/// Max concurrently-running actions per connection. A hostile peer can't spawn
/// unbounded tasks by flooding `call` frames — excess calls are rejected (`429`)
/// immediately rather than queued, so a suspended `client_call` can't deadlock
/// the resume path. Generous: one connection rarely runs this many at once.
const MAX_INFLIGHT_CALLS: usize = 512;

/// A live duplex session over one connection.
pub struct Session<G, R> {
    router: Arc<Router<G, R>>,
    req: R,
    client: ClientHandle,
    out_tx: mpsc::Sender<Value>,
    out_rx: Option<mpsc::Receiver<Value>>,
    inflight: Arc<Semaphore>,
}

impl<G, R> Session<G, R>
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(router: Arc<Router<G, R>>, req: R) -> Self {
        let (out_tx, out_rx) = mpsc::channel(OUTGOING_BUFFER);
        let client = ClientHandle::connected(out_tx.clone(), router.client_call_timeout);
        Self {
            router,
            req,
            client,
            out_tx,
            out_rx: Some(out_rx),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT_CALLS)),
        }
    }

    /// Takes the outbound frame receiver — forward these to your socket. Call once.
    ///
    /// # Panics
    /// Panics if called more than once.
    pub const fn outgoing(&mut self) -> mpsc::Receiver<Value> {
        self.out_rx.take().expect("Session::outgoing already taken")
    }

    /// Handles one inbound frame:
    /// - `{ kind: "call", id, action, params }` → runs the action (unary or
    ///   stream), emitting `result`/`end` frames tagged with `id`.
    /// - `{ kind: "client_result", id, data | error }` → resolves a pending
    ///   `client_call`.
    pub fn handle(&self, frame: &Value) {
        match frame.get("kind").and_then(Value::as_str) {
            Some("call") => self.spawn_call(frame),
            Some("client_result") => self.resolve_client(frame),
            _ => {}
        }
    }

    fn spawn_call(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        let name = frame
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = frame.get("params").cloned().unwrap_or(Value::Null);

        // Bound concurrent actions per connection; reject (not queue) when full
        // so a hostile peer can't spawn unbounded tasks and a suspended
        // `client_call` can't deadlock waiting on a permit.
        let Ok(permit) = Arc::clone(&self.inflight).try_acquire_owned() else {
            let _ = self.out_tx.try_send(result_frame(
                id,
                Err(Error::new(429, "too many in-flight requests")),
            ));
            return;
        };

        let action = self.router.actions.get(name.as_str()).cloned();
        let stream = self.router.streams.get(name.as_str()).cloned();
        let app = Arc::clone(&self.router.app);
        let req = self.req.clone();
        let client = self.client.clone();
        let tx = self.out_tx.clone();
        // Every error this connection emits passes through the router's hook
        // (redact/rewrite) before hitting the wire — same as the request path.
        let on_error = Arc::clone(&self.router.on_error);

        tokio::spawn(async move {
            let _permit = permit; // released when this action's task ends
            let cx = Cx { app, req, client };
            if let Some(action) = action {
                let result = action.dispatch(&cx, params).await.map_err(|e| on_error(e));
                let _ = tx.send(result_frame(id, result)).await;
            } else if let Some(stream) = stream {
                let mut items = stream.dispatch(&cx, params);
                while let Some(item) = items.next().await {
                    let item = item.map_err(|e| on_error(e));
                    let is_err = item.is_err();
                    // Bounded send: applies backpressure to a slow client;
                    // errors only once the receiver (socket) is gone.
                    if tx.send(result_frame(id, item)).await.is_err() || is_err {
                        return;
                    }
                }
                let _ = tx.send(json!({ "kind": "end", "id": id })).await;
            } else {
                let _ = tx
                    .send(result_frame(id, Err(on_error(Error::not_found(&name)))))
                    .await;
            }
        });
    }

    fn resolve_client(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        let result = frame.get("error").map_or_else(
            || Ok(frame.get("data").cloned().unwrap_or(Value::Null)),
            |error| {
                let code = error
                    .get("code")
                    .and_then(Value::as_u64)
                    .and_then(|c| u16::try_from(c).ok())
                    .unwrap_or(500);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("client error");
                Err(Error::new(code, message))
            },
        );
        self.client.resolve(id, result);
    }
}

fn result_frame(id: u64, result: Result<Value, Error>) -> Value {
    match result {
        Ok(data) => json!({ "kind": "result", "id": id, "status": "ok", "data": data }),
        Err(error) => {
            let body = serde_json::to_value(ErrorBody::from(error)).unwrap_or(Value::Null);
            json!({ "kind": "result", "id": id, "status": "error", "error": body })
        }
    }
}
