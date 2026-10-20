//! Two `PostgresBroker` nodes against a real, embedded Postgres. One test
//! boots one server (`postgresql_embedded` downloads a Postgres binary on
//! first run, cached afterward), so the scenarios run in sequence:
//! cross-node publish (incl. `()` params) in order, oversized `413`, device
//! calls (owner checked), device supersede + `close_user` across nodes,
//! forged and replayed NOTIFYs ignored, listener kill → reset → recovery.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde_json::{Value, json};
use stakit_pubsub_postgres::{DEFAULT_CHANNEL, PostgresBroker};
use stakit_router::{ClientAction, CloseReason, Cx, Hub, Identity, Router, Session};
use tokio::sync::mpsc::Receiver;

#[path = "../../orm/tests/support/embedded_pg.rs"]
mod embedded_pg;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

struct Note;
impl ClientAction for Note {
    type Params = String;
    type Return = ();
    const NAME: &'static str = "note";
}

struct Ping;
impl ClientAction for Ping {
    type Params = ();
    type Return = String;
    const NAME: &'static str = "ping";
}

type Open = (Arc<Session<(), ()>>, Receiver<Bytes>);

async fn node(url: &str) -> (Hub, Arc<Router<(), ()>>) {
    let pool = sqlx::PgPool::connect(url).await.expect("connect");
    let broker = PostgresBroker::connect(pool, SECRET).await.expect("listen");
    let hub = Hub::with_call_timeout(broker, Duration::from_secs(1));
    let router = Router::builder().ctx(()).hub(hub.clone()).build();
    (hub, Arc::new(router))
}

fn open(router: &Arc<Router<(), ()>>, topic: &str) -> Open {
    let mut session = router.session(());
    let outgoing = session.outgoing().expect("outgoing");
    session.subscribe(topic).expect("subscribe");
    (Arc::new(session), outgoing)
}

fn device(router: &Arc<Router<(), ()>>, user: &str, id: &str) -> Open {
    let mut session = router.session(());
    let outgoing = session.outgoing().expect("outgoing");
    let identity = Identity::user(user).with_device(id);
    session.authenticate((), &identity).expect("authenticate");
    (Arc::new(session), outgoing)
}

async fn next(outgoing: &mut Receiver<Bytes>) -> Option<Value> {
    let bytes = tokio::time::timeout(Duration::from_secs(5), outgoing.recv())
        .await
        .ok()??;
    Some(serde_json::from_slice(&bytes).expect("json frame"))
}

async fn silent(outgoing: &mut Receiver<Bytes>) -> bool {
    tokio::time::timeout(Duration::from_millis(400), outgoing.recv())
        .await
        .is_err()
}

/// Answers every `client_call` with `"here"`.
fn answer_calls(open: Open) {
    let (session, mut outgoing) = open;
    tokio::spawn(async move {
        while let Some(bytes) = outgoing.recv().await {
            let frame: Value = serde_json::from_slice(&bytes).expect("json frame");
            if frame["kind"] == "client_call" {
                session
                    .handle(&json!({ "kind": "client_result", "id": frame["id"], "data": "here" }));
            }
        }
    });
}

async fn notify(pool: &sqlx::PgPool, payload: &str) {
    sqlx::query("select pg_notify($1, $2)")
        .bind(DEFAULT_CHANNEL)
        .bind(payload)
        .execute(pool)
        .await
        .expect("notify");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_broker_links_nodes() {
    let (_postgres, url) = embedded_pg::boot("pubsub_test").await;
    let (hub_a, router_a) = node(&url).await;
    let (hub_b, router_b) = node(&url).await;
    let admin = sqlx::PgPool::connect(&url).await.expect("connect");

    // ── publish crosses nodes, in order, `()` params included ──
    let (listener, mut on_b) = open(&router_b, "room:1");
    let (_local, mut on_a) = open(&router_a, "room:1");
    for index in 0..50 {
        hub_a
            .publish::<Note>("room:1", &format!("m{index}"))
            .await
            .unwrap();
    }
    for index in 0..50 {
        let frame = next(&mut on_b).await.expect("cross-node publish");
        assert_eq!(frame["params"], format!("m{index}"));
        assert_eq!(frame["seq"], index + 1);
        assert_eq!(next(&mut on_a).await.expect("local")["seq"], index + 1);
    }
    let (_unit, mut on_unit) = open(&router_b, "room:unit");
    hub_a.publish::<Ping>("room:unit", &()).await.unwrap();
    assert_eq!(
        next(&mut on_unit).await.expect("unit publish")["params"],
        Value::Null
    );

    // ── oversized: rejected before anyone, local or remote, gets it ──
    let error = hub_a
        .publish::<Note>("room:1", &"x".repeat(9000))
        .await
        .unwrap_err();
    assert_eq!(error.code, 413);
    assert!(silent(&mut on_a).await && silent(&mut on_b).await);

    // ── forged and replayed NOTIFYs are ignored ──
    let unsigned = r#"{"o":1,"n":0,"t":0,"m":"00","b":{"p":{"topic":"room:1","p":{"name":"note","params":"forged"}}}}"#;
    notify(&admin, unsigned).await;
    let mut tap = sqlx::postgres::PgListener::connect_with(&admin)
        .await
        .expect("tap");
    tap.listen(DEFAULT_CHANNEL).await.expect("listen");
    hub_a
        .publish::<Note>("room:1", &"real".to_owned())
        .await
        .unwrap();
    let captured = tap.recv().await.expect("captured").payload().to_owned();
    assert_eq!(next(&mut on_b).await.expect("real")["params"], "real");
    notify(&admin, &captured).await;
    hub_a
        .publish::<Note>("room:1", &"after".to_owned())
        .await
        .unwrap();
    let frame = next(&mut on_b).await.expect("after");
    assert_eq!(
        frame["params"], "after",
        "a forged or replayed NOTIFY got through"
    );
    drop(tap);

    // ── device calls: one target, owner checked ──
    answer_calls(device(&router_b, "alice", "d1"));
    let cx = Cx::test((), ()).with_hub(hub_a.clone());
    let reply = cx
        .client_call_device::<Ping>("alice", "d1", ())
        .await
        .unwrap();
    assert_eq!(reply, "here");
    let wrong_owner = cx.client_call_device::<Ping>("mallory", "d1", ()).await;
    assert_eq!(wrong_owner.unwrap_err().code, 504);

    // ── a device reconnecting on another node supersedes the old session ──
    let (old_phone, _old_out) = device(&router_b, "bob", "phone");
    let _new_phone = device(&router_a, "bob", "phone");
    let reason = tokio::time::timeout(Duration::from_secs(5), old_phone.closed()).await;
    assert_eq!(
        reason.expect("old device not superseded"),
        CloseReason::Superseded
    );

    // ── close_user reaches the other node ──
    let (carol, _carol_out) = device(&router_b, "carol", "laptop");
    hub_a.close_user("carol").await.unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), carol.closed()).await;
    assert_eq!(reason.expect("carol not closed"), CloseReason::Revoked);

    // ── B drops a topic once its last local subscriber leaves ──
    drop(listener);
    assert_eq!(hub_b.subscriber_count("room:1"), 0);

    // ── a killed listener resets subscribers, then delivery recovers ──
    let (_watcher, mut watched) = open(&router_b, "room:2");
    sqlx::query("select pg_terminate_backend(pid) from pg_stat_activity where query ilike 'listen%' and pid <> pg_backend_pid()")
        .execute(&admin)
        .await
        .expect("terminate listeners");
    let reset = next(&mut watched).await.expect("reset frame");
    assert_eq!(reset["kind"], "reset");
    assert_eq!(reset["topic"], "room:2");
    let mut recovered = false;
    for _ in 0..40 {
        hub_a
            .publish::<Note>("room:2", &"again".to_owned())
            .await
            .unwrap();
        if let Some(frame) = next(&mut watched).await
            && frame["kind"] == "publish"
        {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "publish never arrived after reconnect");
}
