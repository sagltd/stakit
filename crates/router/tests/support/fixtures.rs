//! Fixtures shared by the router integration tests and benches: the one-call
//! payload shape, a counting stream action, an admin guard and a session
//! opener. Each test binary uses a subset.

#![allow(dead_code, unreachable_pub)]

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::Receiver;

use stakit_model::Model;
use stakit_router::{Cx, Error, Middleware, Router, Session, action, err};

/// One-call object payload `{ action: params }`.
pub fn payload(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    Value::Object(map)
}

/// Params of [`count`].
#[derive(Model, Serialize, Deserialize)]
pub struct Count {
    pub n: u64,
}

/// Streams `0..n` (no ctx needed, so it registers on any router).
#[action(stream)]
pub fn count(params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    futures::stream::iter((0..params.n).map(Ok))
}

/// A request context that knows whether its caller is an admin.
pub trait Admin {
    fn is_admin(&self) -> bool;
}

/// Guard that rejects non-admin callers with `403`.
pub struct RequireAdmin;

impl<G, R> Middleware<G, R> for RequireAdmin
where
    G: Send + Sync + 'static,
    R: Admin + Send + Sync + 'static,
{
    async fn before(&self, cx: &Cx<G, R>) -> Result<(), Error> {
        if cx.req.is_admin() {
            Ok(())
        } else {
            Err(err!(403, "admin only"))
        }
    }
}

/// Opens a websocket-style session and takes its outgoing frame channel.
pub fn open_session<G, R>(
    router: &Arc<Router<G, R>>,
    req: R,
) -> (Arc<Session<G, R>>, Receiver<Value>)
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    let mut session = router.session(req);
    let outgoing = session.outgoing();
    (Arc::new(session), outgoing)
}

/// The next outgoing frame; fails the test if none arrives within 2s.
pub async fn next_frame(outgoing: &mut Receiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
        .await
        .expect("no frame within 2s")
        .expect("session closed")
}
