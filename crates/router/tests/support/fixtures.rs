//! Fixtures shared by the router integration tests and benches: the one-call
//! payload shape, a counting stream action, an admin guard and a session
//! opener. Each test binary uses a subset.

#![allow(dead_code, unreachable_pub)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
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
) -> (Arc<Session<G, R>>, Receiver<Bytes>)
where
    G: Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    let mut session = router.session(req);
    let outgoing = session.outgoing().expect("outgoing taken once");
    (Arc::new(session), outgoing)
}

/// The next outgoing frame; fails the test if none arrives within 2s.
pub async fn next_frame(outgoing: &mut Receiver<Bytes>) -> Value {
    let bytes = tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
        .await
        .expect("no frame within 2s")
        .expect("session closed");
    serde_json::from_slice(&bytes).expect("frame is JSON")
}

/// A realistic per-request context: what an adapter typically keeps per
/// socket (cheap to clone — sessions clone it into every call).
pub type Ctx = Arc<RequestCtx>;

/// The request context behind [`Ctx`].
pub struct RequestCtx {
    pub user_id: String,
    pub headers: Vec<(String, String)>,
}

/// A request context the size of a real one (a few headers).
pub fn request_ctx(index: usize) -> Ctx {
    let headers = ["user-agent", "accept-language", "x-request-id", "origin"]
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                format!("value-{index}-{}", "x".repeat(24)),
            )
        })
        .collect();
    Arc::new(RequestCtx {
        user_id: format!("user{index}"),
        headers,
    })
}

/// An authenticated idle session (user + device auto channels) with one more
/// subscription (`room:shared`), as the memory budget test and the alloc
/// benches open them. Needs a Tokio context (the session spawns tasks).
pub fn idle_session(
    router: &Arc<Router<(), Ctx>>,
    index: usize,
) -> (Session<(), Ctx>, Receiver<Bytes>) {
    let ctx = request_ctx(index);
    let mut session = router.session(Arc::clone(&ctx));
    let outgoing = session.outgoing().expect("outgoing taken once");
    let identity =
        stakit_router::Identity::user(&ctx.user_id).with_device(format!("device{index}"));
    session.authenticate(ctx, &identity).unwrap();
    session.subscribe("room:shared").unwrap();
    (session, outgoing)
}
