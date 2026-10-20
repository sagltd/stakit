//! Request frames that spawn work: `call`, `auth`, `subscribe`, plus
//! `client_result` replies.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::{FutureExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::OwnedSemaphorePermit;

use super::{CloseReason, Session, encode, result_frame};
use crate::Error;
use crate::action::panicked;
use crate::pubsub::hub::{Cursor, Subscribe};

/// Attempts to subscribe when a kick keeps landing during `authorize`.
const SUBSCRIBE_ATTEMPTS: usize = 3;

/// Longest client-supplied error message forwarded to the caller.
const MAX_CLIENT_ERROR_CHARS: usize = 256;

impl<G, R> Session<G, R>
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    /// A task slot, or `None` (after answering `429`) when the session is full.
    fn permit(&self, id: u64) -> Option<OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.shared.inflight).try_acquire_owned().ok();
        if permit.is_none() {
            self.shared
                .reply_now(id, Err(Error::new(429, "too many in-flight requests")));
        }
        permit
    }

    pub(super) fn spawn_call(&self, id: u64, frame: &Value) {
        let Some(permit) = self.permit(id) else {
            return;
        };
        let name = frame
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        let router = Arc::clone(&self.shared.router);
        let cx = self.shared.cx();
        let tx = self.shared.out_tx.clone();

        tokio::spawn(async move {
            let _permit = permit; // released when this action's task ends
            // Every error this connection emits passes through the router's hook
            // (redact/rewrite) before hitting the wire — same as the request path.
            let on_error = &router.on_error;
            if let Some(action) = router.actions.get(name.as_str()) {
                let result = action.dispatch(&cx, params).await.map_err(|e| on_error(e));
                let _ = tx.send(result_frame(id, result)).await;
            } else if let Some(stream) = router.streams.get(name.as_str()) {
                let mut items = stream.dispatch(&cx, params);
                while let Some(item) = items.next().await {
                    // An item says `more: true`; an error ends the stream.
                    let (frame, is_err) = match item {
                        Ok(data) => (stream_item_frame(id, &data), false),
                        Err(error) => (result_frame(id, Err(on_error(error))), true),
                    };
                    // Bounded send: applies backpressure to a slow client;
                    // errors only once the receiver (socket) is gone.
                    if tx.send(frame).await.is_err() || is_err {
                        return;
                    }
                }
                let _ = tx.send(encode(&json!({ "kind": "end", "id": id }))).await;
            } else {
                let error = on_error(Error::not_found(&name));
                let _ = tx.send(result_frame(id, Err(error))).await;
            }
        });
    }

    pub(super) fn spawn_auth(&self, id: u64, frame: &Value) {
        let shared = &self.shared;
        let Some(hook) = shared.router.auth.clone() else {
            shared.reply_now(
                id,
                Err(Error::coded(
                    404,
                    "not_found",
                    "authentication is not configured",
                )),
            );
            return;
        };
        // An authenticated session, or one with an `auth` already in flight,
        // just gets 409 — never closed by a duplicate frame.
        let claimed = shared
            .authenticating
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if shared.is_authenticated() || !claimed {
            if claimed {
                shared.authenticating.store(false, Ordering::Release);
            }
            shared.reply_now(
                id,
                Err(Error::coded(
                    409,
                    "conflict",
                    "session already authenticated",
                )),
            );
            return;
        }
        let Some(permit) = self.permit(id) else {
            shared.authenticating.store(false, Ordering::Release);
            return;
        };
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        let shared = Arc::clone(shared);
        tokio::spawn(async move {
            let _permit = permit;
            let attempt = hook(Arc::clone(&shared.router.app), shared.req(), params);
            let outcome = AssertUnwindSafe(attempt)
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| Err(panicked(&*panic)))
                .and_then(|(req, identity)| shared.authenticate(req, &identity));
            shared.authenticating.store(false, Ordering::Release);
            match outcome {
                Ok(()) => shared.reply_now(id, Ok(Value::Null)),
                Err(error) => {
                    let reason = if error.code == 429 {
                        CloseReason::TooManyConnections
                    } else {
                        CloseReason::Unauthorized
                    };
                    shared.reply_now(id, Err(error));
                    shared.close(reason);
                }
            }
        });
    }

    pub(super) fn spawn_subscribe(&self, id: u64, frame: &Value) {
        let shared = &self.shared;
        let name = frame
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(channel) = shared.router.channels.get(name).cloned() else {
            shared.reply_now(id, Err(Error::coded(404, "not_found", "unknown channel")));
            return;
        };
        let Some(permit) = self.permit(id) else {
            return;
        };
        let key = frame.get("key").cloned().unwrap_or(Value::Null);
        let shared = Arc::clone(shared);
        tokio::spawn(async move {
            let _permit = permit;
            let cx = shared.cx();
            let max = shared.router.session_config.max_subscriptions;
            let subscribe = async {
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    let resolved = channel.resolve(&cx, key.clone()).await?;
                    let topic = resolved.topic.as_str();
                    // The ack is queued under the hub's lock, ahead of any publish.
                    let ack = |cursor: Cursor| {
                        let data =
                            json!({ "topic": topic, "epoch": cursor.epoch, "seq": cursor.seq });
                        result_frame(id, Ok(data))
                    };
                    let request = Subscribe {
                        explicit: true,
                        max,
                        mark: Some(resolved.mark),
                        ack: Some(&ack),
                    };
                    match shared.router.hub.subscribe(shared.conn, topic, &request) {
                        Err(error)
                            if error.kind == "subscription_revoked"
                                && attempts < SUBSCRIBE_ATTEMPTS => {}
                        outcome => return outcome.map(|_| ()),
                    }
                }
            };
            let outcome = AssertUnwindSafe(subscribe)
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| Err(panicked(&*panic)));
            if let Err(error) = outcome {
                shared.reply_now(id, Err(error));
            }
        });
    }

    pub(super) fn resolve_client(&self, id: u64, frame: &Value) {
        let result = frame.get("error").map_or_else(
            || Ok(frame.get("data").cloned().unwrap_or(Value::Null)),
            |error| Err(client_error(error)),
        );
        self.shared.client.resolve(id, result);
    }
}

/// A stream item: a `result` with `more: true` (the stream ends with `end`
/// or an error `result`), so clients can tell it from a unary reply.
fn stream_item_frame(id: u64, data: &Value) -> bytes::Bytes {
    encode(&json!({ "kind": "result", "id": id, "status": "ok", "data": data, "more": true }))
}

/// A client's error reply, sanitized before it reaches whoever made the call:
/// the code is clamped to 4xx/5xx and the message truncated.
fn client_error(error: &Value) -> Error {
    let code = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (400..=599).contains(code))
        .unwrap_or(500);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("client error");
    let message: String = message.chars().take(MAX_CLIENT_ERROR_CHARS).collect();
    Error::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_error_codes_are_clamped() {
        assert_eq!(
            client_error(&json!({ "code": 200, "message": "x" })).code,
            500
        );
        assert_eq!(client_error(&json!({ "code": 99_999 })).code, 500);
        assert_eq!(client_error(&json!({ "code": 404 })).code, 404);
    }

    #[test]
    fn client_error_messages_are_truncated() {
        let long = "x".repeat(10_000);
        let error = client_error(&json!({ "code": 400, "message": long }));
        assert_eq!(error.message.chars().count(), MAX_CLIENT_ERROR_CHARS);
    }
}
