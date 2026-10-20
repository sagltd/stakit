use std::sync::atomic::AtomicUsize;

use futures::executor::block_on;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::*;

struct Note;

impl ClientAction for Note {
    type Params = String;
    type Return = ();
    const NAME: &'static str = "note";
}

struct Ping;

impl ClientAction for Ping {
    type Params = ();
    type Return = ();
    const NAME: &'static str = "ping";
}

struct TestConn {
    id: ConnId,
    rx: mpsc::Receiver<Bytes>,
    closed: watch::Receiver<Option<CloseReason>>,
}

impl TestConn {
    fn next(&mut self) -> Option<Value> {
        let bytes = self.rx.try_recv().ok()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    }

    fn close_reason(&self) -> Option<CloseReason> {
        *self.closed.borrow()
    }
}

fn connect(hub: &Hub, capacity: usize) -> TestConn {
    let (tx, rx) = mpsc::channel(capacity);
    let client = ClientHandle::connected(tx.clone(), Duration::from_secs(1));
    let signal = Arc::new(watch::Sender::new(None));
    let closed = signal.subscribe();
    let id = hub.register(tx, client, signal);
    TestConn { id, rx, closed }
}

const EXPLICIT: Subscribe<'static> = Subscribe {
    explicit: true,
    max: 8,
    mark: None,
    ack: None,
};

fn sub(hub: &Hub, conn: &TestConn, topic: &str) -> Cursor {
    hub.subscribe(conn.id, topic, &EXPLICIT).unwrap()
}

fn publish(hub: &Hub, topic: &str, text: &str) {
    block_on(hub.publish::<Note>(topic, &text.to_owned())).unwrap();
}

/// A broker whose limit check and hand-off can be made to fail.
#[derive(Default)]
struct FlakyBroker {
    reject: bool,
    fail: bool,
    published: Arc<AtomicUsize>,
}

impl Broker for FlakyBroker {
    fn attach(&self, _sink: BrokerSink) {}
    fn subscribe(&self, _topic: &str) {}
    fn unsubscribe(&self, _topic: &str) {}
    fn validate(&self, _topic: &str, _payload: &[u8]) -> Result<(), Error> {
        if self.reject {
            return Err(Error::coded(413, "payload_too_large", "too big"));
        }
        Ok(())
    }
    fn publish<'a>(&'a self, _topic: &'a str, _payload: Bytes) -> BoxFuture<'a, Result<(), Error>> {
        self.published.fetch_add(1, Ordering::Relaxed);
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                return Err(Error::new(503, "broker down"));
            }
            Ok(())
        })
    }
    fn call<'a>(
        &'a self,
        _topic: &'a str,
        _payload: Bytes,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async { Err(Error::new(504, "timeout")) })
    }
}

// ── publish ──────────────────────────────────────────────────────────────────

#[test]
fn publish_reaches_subscriber_with_seq_and_epoch() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    let cursor = sub(&hub, &conn, "room:1");
    assert_eq!(cursor.seq, 0);

    publish(&hub, "room:1", "hi");

    assert_eq!(
        conn.next().unwrap(),
        json!({ "kind": "publish", "topic": "room:1", "name": "note", "seq": 1, "epoch": cursor.epoch, "params": "hi" })
    );
}

#[test]
fn null_params_are_delivered_locally_and_from_the_broker() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    sub(&hub, &conn, "room:1");
    block_on(hub.publish::<Ping>("room:1", &())).unwrap();
    assert_eq!(conn.next().unwrap()["params"], Value::Null);

    hub.inner
        .deliver("room:1", br#"{"name":"ping","params":null}"#);
    assert_eq!(conn.next().unwrap()["seq"], 2);
}

#[test]
fn seq_increments_per_publish() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    sub(&hub, &conn, "room:1");
    publish(&hub, "room:1", "a");
    publish(&hub, "room:1", "b");
    let seqs: Vec<Value> = [conn.next().unwrap(), conn.next().unwrap()]
        .iter()
        .map(|frame| frame["seq"].clone())
        .collect();
    assert_eq!(seqs, [json!(1), json!(2)]);
}

/// Publishes `"x"` on a hub over `broker` with one subscriber.
fn publish_through(broker: FlakyBroker) -> (Error, Option<Value>) {
    let hub = Hub::new(broker);
    let mut conn = connect(&hub, 8);
    sub(&hub, &conn, "room:1");
    let error = block_on(hub.publish::<Note>("room:1", &"x".to_owned())).unwrap_err();
    (error, conn.next())
}

#[test]
fn oversized_publish_reaches_nobody() {
    let (error, frame) = publish_through(FlakyBroker {
        reject: true,
        ..FlakyBroker::default()
    });
    assert_eq!(error.code, 413);
    assert!(frame.is_none());
}

#[test]
fn broker_failure_still_delivers_locally() {
    let (error, frame) = publish_through(FlakyBroker {
        fail: true,
        ..FlakyBroker::default()
    });
    assert_eq!(error.code, 503);
    assert_eq!(frame.unwrap()["params"], "x");
}

#[test]
fn publish_without_subscribers_is_a_no_op() {
    let hub = Hub::default();
    publish(&hub, "nobody", "x");
    assert_eq!(hub.subscriber_count("nobody"), 0);
}

#[test]
fn malformed_or_unknown_broker_payload_is_ignored() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    sub(&hub, &conn, "room:1");
    hub.inner.deliver("room:1", b"not json");
    hub.inner.deliver("room:1", br#"{"name":"x"}"#);
    hub.inner.deliver("other", br#"{"name":"x","params":1}"#);
    assert!(conn.next().is_none());
}

// ── subscribe ────────────────────────────────────────────────────────────────

#[test]
fn subscribe_is_idempotent() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    sub(&hub, &conn, "room:1");
    sub(&hub, &conn, "room:1");
    publish(&hub, "room:1", "once");
    assert!(conn.next().is_some());
    assert!(conn.next().is_none(), "delivered twice");
}

#[test]
fn ack_is_queued_before_any_later_publish() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    let ack = |cursor: Cursor| Bytes::from(format!(r#"{{"ack":{}}}"#, cursor.seq));
    let request = Subscribe {
        ack: Some(&ack),
        ..EXPLICIT
    };
    hub.subscribe(conn.id, "room:1", &request).unwrap();
    publish(&hub, "room:1", "after");
    assert_eq!(conn.next().unwrap(), json!({ "ack": 0 }));
    assert_eq!(conn.next().unwrap()["seq"], 1);
}

#[test]
fn ack_on_a_full_queue_closes_the_session() {
    let hub = Hub::default();
    let conn = connect(&hub, 1);
    sub(&hub, &conn, "room:0");
    publish(&hub, "room:0", "fills the queue");
    let ack = |_: Cursor| Bytes::from_static(b"{}");
    let request = Subscribe {
        ack: Some(&ack),
        ..EXPLICIT
    };
    let _ = hub.subscribe(conn.id, "room:1", &request);
    assert_eq!(conn.close_reason(), Some(CloseReason::SlowConsumer));
}

#[test]
fn unsubscribe_stops_delivery_and_drops_topic() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    sub(&hub, &conn, "room:1");
    assert!(hub.unsubscribe(conn.id, "room:1"));
    assert!(!hub.unsubscribe(conn.id, "room:1"));
    publish(&hub, "room:1", "x");
    assert!(conn.next().is_none());
    assert_eq!(hub.subscriber_count("room:1"), 0);
}

#[test]
fn recreated_topic_gets_a_new_epoch() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    let first = sub(&hub, &conn, "room:1");
    hub.unsubscribe(conn.id, "room:1");
    let second = sub(&hub, &conn, "room:1");
    assert_ne!(first.epoch, second.epoch);
    assert!(second.epoch <= state::JS_SAFE);
}

#[test]
fn full_queue_closes_the_slow_consumer() {
    let hub = Hub::default();
    let mut slow = connect(&hub, 1);
    let mut fast = connect(&hub, 8);
    sub(&hub, &slow, "room:1");
    sub(&hub, &fast, "room:1");

    publish(&hub, "room:1", "1");
    publish(&hub, "room:1", "2");

    assert_eq!(slow.close_reason(), Some(CloseReason::SlowConsumer));
    assert_eq!(hub.subscriber_count("room:1"), 1);
    assert_eq!(hub.connection_count(), 1);
    assert!(fast.next().is_some() && fast.next().is_some());
    assert!(
        slow.next().is_some(),
        "frames queued before the close stay readable"
    );
}

#[test]
fn explicit_subscriptions_are_capped() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    let capped = Subscribe { max: 1, ..EXPLICIT };
    hub.subscribe(conn.id, "a", &capped).unwrap();
    let error = hub.subscribe(conn.id, "b", &capped).unwrap_err();
    assert_eq!(error.code, 429);
    let server_side = Subscribe {
        explicit: false,
        ..capped
    };
    hub.subscribe(conn.id, "c", &server_side).unwrap();
    hub.unsubscribe(conn.id, "a");
    hub.subscribe(conn.id, "b", &capped).unwrap();
}

#[test]
fn subscribe_on_closed_connection_fails() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    hub.remove(conn.id);
    assert_eq!(
        hub.subscribe(conn.id, "a", &EXPLICIT).unwrap_err().code,
        410
    );
}

#[test]
fn kick_during_authorize_refuses_the_subscribe() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    let mark = hub.revocation_mark("room:1");
    block_on(hub.kick_user("room:1", "alice")).unwrap();
    let request = Subscribe {
        mark: Some(mark),
        ..EXPLICIT
    };
    let error = hub.subscribe(conn.id, "room:1", &request).unwrap_err();
    assert_eq!(error.kind, "subscription_revoked");
    let fresh = Subscribe {
        mark: Some(hub.revocation_mark("room:1")),
        ..EXPLICIT
    };
    hub.subscribe(conn.id, "room:1", &fresh).unwrap();
}

// ── identity ─────────────────────────────────────────────────────────────────

#[test]
fn authenticate_subscribes_user_and_device_channels() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    hub.authenticate(conn.id, &Identity::user("u1").with_device("d1"), 4)
        .unwrap();
    assert_eq!(hub.subscriber_count("user:id=u1"), 1);
    assert_eq!(hub.subscriber_count("device:user=u1:id=d1"), 1);
    publish(&hub, "user:id=u1", "hello");
    assert_eq!(conn.next().unwrap()["topic"], "user:id=u1");
}

#[test]
fn device_without_user_is_rejected() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    let identity = Identity {
        user: None,
        device: Some("d1".to_owned()),
    };
    assert_eq!(
        hub.authenticate(conn.id, &identity, 4).unwrap_err().code,
        400
    );
}

#[test]
fn failed_authenticate_changes_nothing() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    let too_long = Identity::user("u1").with_device("d".repeat(MAX_TOPIC));
    assert_eq!(
        hub.authenticate(conn.id, &too_long, 1).unwrap_err().code,
        400
    );
    assert_eq!(hub.subscriber_count("user:id=u1"), 0);
    // The cap slot wasn't taken, and a retry works.
    hub.authenticate(conn.id, &Identity::user("u1"), 1).unwrap();
}

const MAX_TOPIC: usize = crate::MAX_TOPIC_BYTES;

#[test]
fn per_user_connection_cap_is_enforced_and_released() {
    let hub = Hub::default();
    let first = connect(&hub, 8);
    let second = connect(&hub, 8);
    hub.authenticate(first.id, &Identity::user("u1"), 1)
        .unwrap();
    let error = hub
        .authenticate(second.id, &Identity::user("u1"), 1)
        .unwrap_err();
    assert_eq!(error.code, 429);
    hub.remove(first.id);
    hub.authenticate(second.id, &Identity::user("u1"), 1)
        .unwrap();
}

#[test]
fn authenticating_twice_conflicts() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    hub.authenticate(conn.id, &Identity::default(), 4).unwrap();
    let error = hub
        .authenticate(conn.id, &Identity::user("u2"), 4)
        .unwrap_err();
    assert_eq!(error.code, 409);
}

#[test]
fn another_users_device_claim_conflicts() {
    let hub = Hub::default();
    let owner = connect(&hub, 8);
    let thief = connect(&hub, 8);
    hub.authenticate(owner.id, &Identity::user("alice").with_device("d1"), 4)
        .unwrap();
    let error = hub
        .authenticate(thief.id, &Identity::user("mallory").with_device("d1"), 4)
        .unwrap_err();
    assert_eq!(error.kind, "device_conflict");
    assert!(owner.close_reason().is_none());
}

#[test]
fn same_users_new_connection_supersedes_the_device() {
    let hub = Hub::default();
    let old = connect(&hub, 8);
    let new = connect(&hub, 8);
    let identity = Identity::user("alice").with_device("d1");
    hub.authenticate(old.id, &identity, 4).unwrap();
    hub.authenticate(new.id, &identity, 4).unwrap();
    assert_eq!(old.close_reason(), Some(CloseReason::Superseded));
    assert_eq!(hub.subscriber_count("device:user=alice:id=d1"), 1);
}

#[test]
fn device_claim_supersedes_the_holder_on_another_node() {
    let node_a = InMemoryBroker::new();
    let node_b = node_a.link();
    let (hub_a, hub_b) = (Hub::new(node_a), Hub::new(node_b));
    let old = connect(&hub_b, 8);
    let new = connect(&hub_a, 8);
    let identity = Identity::user("alice").with_device("d1");
    hub_b.authenticate(old.id, &identity, 4).unwrap();
    hub_a.authenticate(new.id, &identity, 4).unwrap();
    assert_eq!(old.close_reason(), Some(CloseReason::Superseded));
    assert!(new.close_reason().is_none());
}

#[tokio::test]
async fn device_calls_only_reach_the_owners_device() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    hub.authenticate(conn.id, &Identity::user("alice").with_device("d1"), 4)
        .unwrap();
    let error = hub
        .client_call_device("mallory", "d1", "locate", Value::Null)
        .await
        .unwrap_err();
    assert_eq!(error.code, 404);
}

// ── kicks, revocation, resets ────────────────────────────────────────────────

#[test]
fn kick_user_removes_only_that_users_subscription() {
    let hub = Hub::default();
    let mut alice = connect(&hub, 8);
    let mut bob = connect(&hub, 8);
    hub.authenticate(alice.id, &Identity::user("alice"), 4)
        .unwrap();
    hub.authenticate(bob.id, &Identity::user("bob"), 4).unwrap();
    sub(&hub, &alice, "room:1");
    sub(&hub, &bob, "room:1");

    block_on(hub.kick_user("room:1", "alice")).unwrap();

    assert_eq!(
        alice.next().unwrap(),
        json!({ "kind": "unsubscribed", "topic": "room:1", "reason": "kicked" })
    );
    assert!(bob.next().is_none());
    assert_eq!(hub.subscriber_count("room:1"), 1);
}

#[test]
fn kick_happens_locally_even_when_the_broker_fails() {
    let hub = Hub::new(FlakyBroker {
        fail: true,
        ..FlakyBroker::default()
    });
    let alice = connect(&hub, 8);
    hub.authenticate(alice.id, &Identity::user("alice"), 4)
        .unwrap();
    sub(&hub, &alice, "room:1");
    assert!(block_on(hub.kick_user("room:1", "alice")).is_err());
    assert_eq!(hub.subscriber_count("room:1"), 0);
}

#[test]
fn kick_on_a_full_queue_closes_the_session() {
    let hub = Hub::default();
    let alice = connect(&hub, 1);
    hub.authenticate(alice.id, &Identity::user("alice"), 4)
        .unwrap();
    sub(&hub, &alice, "room:1");
    publish(&hub, "room:1", "fills the queue");
    block_on(hub.kick_user("room:1", "alice")).unwrap();
    assert_eq!(alice.close_reason(), Some(CloseReason::SlowConsumer));
}

#[test]
fn close_user_closes_all_of_their_sessions_on_every_node() {
    let node_a = InMemoryBroker::new();
    let node_b = node_a.link();
    let (hub_a, hub_b) = (Hub::new(node_a), Hub::new(node_b));
    let here = connect(&hub_a, 8);
    let there = connect(&hub_b, 8);
    let bystander = connect(&hub_b, 8);
    hub_a
        .authenticate(here.id, &Identity::user("alice"), 4)
        .unwrap();
    hub_b
        .authenticate(there.id, &Identity::user("alice"), 4)
        .unwrap();
    hub_b
        .authenticate(bystander.id, &Identity::user("bob"), 4)
        .unwrap();

    block_on(hub_a.close_user("alice")).unwrap();

    assert_eq!(here.close_reason(), Some(CloseReason::Revoked));
    assert_eq!(there.close_reason(), Some(CloseReason::Revoked));
    assert!(bystander.close_reason().is_none());
}

#[test]
fn close_device_closes_the_holder_on_another_node() {
    let node_a = InMemoryBroker::new();
    let node_b = node_a.link();
    let (hub_a, hub_b) = (Hub::new(node_a), Hub::new(node_b));
    let phone = connect(&hub_b, 8);
    let laptop = connect(&hub_b, 8);
    hub_b
        .authenticate(phone.id, &Identity::user("alice").with_device("phone"), 4)
        .unwrap();
    hub_b
        .authenticate(laptop.id, &Identity::user("alice").with_device("laptop"), 4)
        .unwrap();
    block_on(hub_a.close_device("alice", "phone")).unwrap();
    assert_eq!(phone.close_reason(), Some(CloseReason::Revoked));
    assert!(laptop.close_reason().is_none());
}

#[test]
fn close_drops_every_subscription_and_signals_reason() {
    let hub = Hub::default();
    let conn = connect(&hub, 8);
    hub.authenticate(conn.id, &Identity::user("u1"), 4).unwrap();
    sub(&hub, &conn, "room:1");
    hub.close(conn.id, CloseReason::Idle);
    assert_eq!(conn.close_reason(), Some(CloseReason::Idle));
    assert_eq!(hub.subscriber_count("room:1"), 0);
    assert_eq!(hub.subscriber_count("user:id=u1"), 0);
    assert_eq!(hub.connection_count(), 0);
}

#[test]
fn broker_reset_drops_explicit_and_resets_auto_subscriptions() {
    let hub = Hub::default();
    let mut conn = connect(&hub, 8);
    hub.authenticate(conn.id, &Identity::user("u1"), 4).unwrap();
    sub(&hub, &conn, "room:1");
    let mark = hub.revocation_mark("room:1");

    BrokerSink {
        hub: Arc::downgrade(&hub.inner),
    }
    .reset();

    let mut frames = [conn.next().unwrap(), conn.next().unwrap()];
    frames.sort_by_key(|frame| frame["kind"].to_string());
    assert_eq!(frames[0]["kind"], "reset");
    assert_eq!(frames[0]["topic"], "user:id=u1");
    assert_eq!(
        frames[1],
        json!({ "kind": "unsubscribed", "topic": "room:1", "reason": "reset" })
    );
    assert_eq!(hub.subscriber_count("room:1"), 0);
    assert_eq!(hub.subscriber_count("user:id=u1"), 1);
    assert_ne!(hub.revocation_mark("room:1"), mark);
}

#[tokio::test]
async fn device_call_without_holder_is_not_found_in_memory() {
    let hub = Hub::default();
    let error = hub
        .client_call_device("u", "d", "ping", Value::Null)
        .await
        .unwrap_err();
    assert_eq!(error.code, 404);
}
