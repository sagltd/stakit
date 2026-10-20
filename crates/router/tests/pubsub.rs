//! Pub/sub over duplex sessions: auth, typed channels, publish from actions,
//! lifecycle (heartbeat / idle / auth window / frame size / caps) and
//! multi-node delivery through a linked `InMemoryBroker`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::Receiver;

use stakit_model::Model;
use stakit_router::{
    Authorize, Channel, ClientAction, CloseReason, Cx, Endpoint, Error, Hub, Identity,
    InMemoryBroker, Router, Session, SessionConfig, action,
};

#[path = "support/fixtures.rs"]
mod fixtures;
use fixtures::{next_frame, open_session};

struct App;

#[derive(Clone, Default)]
struct Req {
    user: Option<String>,
}

#[derive(Model, Serialize, Deserialize)]
struct Room {
    #[validate(min_len = 1, max_len = 32)]
    id: String,
}

impl Channel for Room {
    const NAME: &'static str = "room";
}

impl Authorize<App, Req> for Room {
    async fn authorize(&self, cx: &Cx<App, Req>) -> Result<bool, Error> {
        Ok(cx.req.user.is_some() && self.id != "secret")
    }
}

#[derive(Model, Serialize, Deserialize)]
struct Text {
    text: String,
}

struct Message;
impl ClientAction for Message {
    type Params = Text;
    type Return = ();
    const NAME: &'static str = "message";
}

struct Locate;
impl ClientAction for Locate {
    type Params = ();
    type Return = String;
    const NAME: &'static str = "locate";
}

#[derive(Model, Serialize, Deserialize)]
struct Say {
    room: String,
    text: String,
}

#[action]
async fn say(cx: &Cx<App, Req>, params: Say) -> Result<(), Error> {
    let room = Room { id: params.room };
    cx.publish::<Message>(&room, &Text { text: params.text })
        .await
}

#[derive(Model, Serialize, Deserialize)]
struct Target {
    user: String,
    device: String,
}

#[action]
async fn locate(cx: &Cx<App, Req>, params: Target) -> Result<String, Error> {
    cx.client_call_device::<Locate>(&params.user, &params.device, ())
        .await
}

fn fast_config() -> SessionConfig {
    SessionConfig {
        heartbeat: Duration::from_millis(20),
        idle_timeout: Duration::from_millis(150),
        auth_window: Duration::from_millis(80),
        max_connections_per_user: 2,
        max_frame_bytes: 1024,
        max_subscriptions: 2,
        ..SessionConfig::default()
    }
}

/// Token `"<user>"` or `"<user>/<device>"` authenticates; `"bad"` is rejected.
fn router_on(hub: Hub, config: SessionConfig) -> Arc<Router<App, Req>> {
    let router = Router::builder()
        .ctx(App)
        .hub(hub)
        .register(say)
        .register(locate)
        .channel::<Room>()
        .client_action::<Message>()
        .session_config(config)
        .on_auth(|_app, _req, params: Value| async move {
            let token = params["token"].as_str().unwrap_or_default().to_owned();
            if token == "bad" {
                return Err(Error::coded(401, "unauthorized", "bad token"));
            }
            let (user, device) = token.split_once('/').unwrap_or((token.as_str(), ""));
            let mut identity = Identity::user(user);
            if !device.is_empty() {
                identity = identity.with_device(device);
            }
            Ok((
                Req {
                    user: Some(user.to_owned()),
                },
                identity,
            ))
        })
        .build();
    Arc::new(router)
}

fn router() -> Arc<Router<App, Req>> {
    router_on(Hub::default(), SessionConfig::default())
}

type Open = (Arc<Session<App, Req>>, Receiver<Bytes>);
type Shared = Arc<Router<App, Req>>;

async fn authed(router: &Arc<Router<App, Req>>, token: &str) -> Open {
    let (session, mut outgoing) = open_session(router, Req::default());
    session.handle(&json!({ "kind": "auth", "id": 1, "params": { "token": token } }));
    let reply = next_frame(&mut outgoing).await;
    assert_eq!(reply["status"], "ok", "auth failed: {reply}");
    (session, outgoing)
}

async fn subscribe(open: &mut Open, id: u64, room: &str) -> Value {
    open.0.handle(
        &json!({ "kind": "subscribe", "id": id, "channel": "room", "key": { "id": room } }),
    );
    next_frame(&mut open.1).await
}

async fn closed_within(session: &Session<App, Req>, wait: Duration) -> CloseReason {
    tokio::time::timeout(wait, session.closed())
        .await
        .expect("session not closed in time")
}

// ── auth ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn calls_before_auth_are_rejected() {
    let (session, mut outgoing) = open_session(&router(), Req::default());
    session.handle(
        &json!({ "kind": "call", "id": 5, "action": <say as Endpoint>::ACTION, "params": {} }),
    );
    let reply = next_frame(&mut outgoing).await;
    assert_eq!(reply["id"], 5);
    assert_eq!(reply["error"]["code"], 401);
}

#[tokio::test]
async fn auth_subscribes_the_user_channel() {
    let router = router();
    let (_session, mut outgoing) = authed(&router, "alice").await;
    router
        .hub()
        .publish::<Message>("user:id=alice", &Text { text: "hi".into() })
        .await
        .unwrap();
    let frame = next_frame(&mut outgoing).await;
    assert_eq!(frame["kind"], "publish");
    assert_eq!(frame["topic"], "user:id=alice");
    assert_eq!(frame["params"]["text"], "hi");
}

#[tokio::test]
async fn rejected_auth_replies_then_closes() {
    let (session, mut outgoing) = open_session(&router(), Req::default());
    session.handle(&json!({ "kind": "auth", "id": 1, "params": { "token": "bad" } }));
    assert_eq!(next_frame(&mut outgoing).await["error"]["code"], 401);
    assert_eq!(
        closed_within(&session, Duration::from_secs(1)).await,
        CloseReason::Unauthorized
    );
}

#[tokio::test]
async fn missing_auth_closes_after_the_window() {
    let router = router_on(Hub::default(), fast_config());
    let (session, _outgoing) = open_session(&router, Req::default());
    assert_eq!(
        closed_within(&session, Duration::from_secs(1)).await,
        CloseReason::AuthTimeout
    );
}

#[tokio::test]
async fn per_user_connection_cap_closes_extra_sessions() {
    let router = router_on(Hub::default(), fast_config());
    let _first = authed(&router, "alice").await;
    let _second = authed(&router, "alice").await;
    let (third, mut outgoing) = open_session(&router, Req::default());
    third.handle(&json!({ "kind": "auth", "id": 1, "params": { "token": "alice" } }));
    assert_eq!(next_frame(&mut outgoing).await["error"]["code"], 429);
    assert_eq!(
        closed_within(&third, Duration::from_secs(1)).await,
        CloseReason::TooManyConnections
    );
}

#[tokio::test]
async fn adapter_can_authenticate_directly() {
    let router = router();
    let (session, mut outgoing) = open_session(&router, Req::default());
    session
        .authenticate(
            Req {
                user: Some("bob".into()),
            },
            &Identity::user("bob"),
        )
        .unwrap();
    session
        .handle(&json!({ "kind": "subscribe", "id": 2, "channel": "room", "key": { "id": "r1" } }));
    assert_eq!(next_frame(&mut outgoing).await["status"], "ok");
}

// ── channels ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn subscriber_receives_publish_from_an_action() {
    let router = router();
    let mut listener = authed(&router, "alice").await;
    let ack = subscribe(&mut listener, 2, "r1").await;
    assert_eq!(ack["data"]["topic"], "room:id=r1");
    assert_eq!(ack["data"]["seq"], 0);

    let mut speaker = authed(&router, "bob").await;
    speaker.0.handle(&json!({
        "kind": "call", "id": 3, "action": <say as Endpoint>::ACTION,
        "params": { "room": "r1", "text": "hello" },
    }));
    assert_eq!(next_frame(&mut speaker.1).await["status"], "ok");

    let frame = next_frame(&mut listener.1).await;
    assert_eq!(frame["name"], "message");
    assert_eq!(frame["seq"], 1);
    assert_eq!(frame["epoch"], ack["data"]["epoch"]);
    assert_eq!(frame["params"]["text"], "hello");
}

#[tokio::test]
async fn unauthorized_subscribe_is_forbidden() {
    let mut open = authed(&router(), "alice").await;
    let reply = subscribe(&mut open, 2, "secret").await;
    assert_eq!(reply["error"]["code"], 403);
}

#[tokio::test]
async fn invalid_channel_key_fails_validation() {
    let mut open = authed(&router(), "alice").await;
    assert_eq!(subscribe(&mut open, 2, "").await["error"]["code"], 422);
    open.0
        .handle(&json!({ "kind": "subscribe", "id": 3, "channel": "room", "key": 7 }));
    assert_eq!(next_frame(&mut open.1).await["error"]["code"], 400);
}

#[tokio::test]
async fn unknown_channel_is_not_found() {
    let (session, mut outgoing) = authed(&router(), "alice").await;
    session.handle(
        &json!({ "kind": "subscribe", "id": 2, "channel": "user", "key": { "id": "bob" } }),
    );
    assert_eq!(next_frame(&mut outgoing).await["error"]["code"], 404);
}

#[tokio::test]
async fn subscription_cap_applies_per_session() {
    let router = router_on(Hub::default(), fast_config());
    let mut open = authed(&router, "alice").await;
    assert_eq!(subscribe(&mut open, 2, "a").await["status"], "ok");
    assert_eq!(subscribe(&mut open, 3, "b").await["status"], "ok");
    assert_eq!(subscribe(&mut open, 4, "c").await["error"]["code"], 429);
}

#[tokio::test]
async fn unsubscribe_frame_stops_delivery() {
    let router = router();
    let mut open = authed(&router, "alice").await;
    subscribe(&mut open, 2, "r1").await;
    open.0
        .handle(&json!({ "kind": "unsubscribe", "id": 3, "topic": "room:id=r1" }));
    assert_eq!(next_frame(&mut open.1).await["data"], true);
    assert_eq!(router.hub().subscriber_count("room:id=r1"), 0);
}

#[tokio::test]
async fn dropping_a_session_releases_its_subscriptions() {
    let router = router();
    let mut open = authed(&router, "alice").await;
    subscribe(&mut open, 2, "r1").await;
    drop(open);
    assert_eq!(router.hub().subscriber_count("room:id=r1"), 0);
    assert_eq!(router.hub().connection_count(), 0);
}

// ── lifecycle ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn heartbeat_pings_then_idle_closes_a_silent_client() {
    let router = router_on(Hub::default(), fast_config());
    let (session, mut outgoing) = authed(&router, "alice").await;
    assert_eq!(next_frame(&mut outgoing).await, json!({ "kind": "ping" }));
    assert_eq!(
        closed_within(&session, Duration::from_secs(1)).await,
        CloseReason::Idle
    );
}

#[tokio::test]
async fn pongs_keep_a_session_alive() {
    let router = router_on(Hub::default(), fast_config());
    let (session, _outgoing) = authed(&router, "alice").await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(15)).await;
        session.handle(&json!({ "kind": "pong" }));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(5), session.closed())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn oversized_frame_closes_the_session() {
    let router = router_on(Hub::default(), fast_config());
    let (session, _outgoing) = authed(&router, "alice").await;
    session.handle_bytes(&vec![b' '; 2048]);
    assert_eq!(
        closed_within(&session, Duration::from_secs(1)).await,
        CloseReason::FrameTooLarge
    );
}

// ── multi-node through a linked InMemoryBroker ───────────────────────────────

fn two_nodes() -> (Shared, Shared) {
    let node_a = InMemoryBroker::new();
    let node_b = node_a.link();
    let a = router_on(Hub::new(node_a), SessionConfig::default());
    let b = router_on(
        Hub::with_call_timeout(node_b, Duration::from_secs(1)),
        SessionConfig::default(),
    );
    (a, b)
}

#[tokio::test]
async fn publish_on_one_node_reaches_a_subscriber_on_another() {
    let (a, b) = two_nodes();
    let mut listener = authed(&b, "alice").await;
    subscribe(&mut listener, 2, "r1").await;

    let mut speaker = authed(&a, "bob").await;
    speaker.0.handle(&json!({
        "kind": "call", "id": 3, "action": <say as Endpoint>::ACTION,
        "params": { "room": "r1", "text": "across" },
    }));
    assert_eq!(next_frame(&mut speaker.1).await["status"], "ok");

    let frame = next_frame(&mut listener.1).await;
    assert_eq!(frame["params"]["text"], "across");
    assert_eq!(frame["seq"], 1);
}

#[tokio::test]
async fn node_drops_interest_when_its_last_subscriber_leaves() {
    let node = InMemoryBroker::new().link();
    let router = router_on(Hub::new(node.clone()), SessionConfig::default());
    let mut first = authed(&router, "alice").await;
    let mut second = authed(&router, "bob").await;
    subscribe(&mut first, 2, "r1").await;
    subscribe(&mut second, 2, "r1").await;
    assert!(node.has_interest("room:id=r1"));
    assert!(node.has_interest("user:id=alice"));

    drop(first);
    assert!(node.has_interest("room:id=r1"), "bob still listens");
    assert!(!node.has_interest("user:id=alice"));
    drop(second);
    assert!(!node.has_interest("room:id=r1"));
}

/// Asks `caller`'s node to call `user`'s `device` (call id 9).
fn locate_on(caller: &Open, user: &str, device: &str) {
    caller.0.handle(&json!({
        "kind": "call", "id": 9, "action": <locate as Endpoint>::ACTION,
        "params": { "user": user, "device": device },
    }));
}

#[tokio::test]
async fn device_call_reaches_a_device_on_another_node() {
    let (a, b) = two_nodes();
    let (device, mut device_out) = authed(&b, "alice/phone-1").await;

    let mut caller = authed(&a, "admin").await;
    locate_on(&caller, "alice", "phone-1");

    let call = next_frame(&mut device_out).await;
    assert_eq!(call["kind"], "client_call");
    assert_eq!(call["name"], "locate");
    device.handle(&json!({ "kind": "client_result", "id": call["id"], "data": "at home" }));

    let reply = next_frame(&mut caller.1).await;
    assert_eq!(reply["id"], 9);
    assert_eq!(reply["data"], "at home");
}

#[tokio::test]
async fn device_call_to_an_absent_device_is_not_found() {
    let (a, _b) = two_nodes();
    let mut caller = authed(&a, "admin").await;
    locate_on(&caller, "alice", "ghost");
    assert_eq!(next_frame(&mut caller.1).await["error"]["code"], 404);
}

#[tokio::test]
async fn kick_on_one_node_unsubscribes_the_user_on_another() {
    let (a, b) = two_nodes();
    let mut listener = authed(&b, "alice").await;
    subscribe(&mut listener, 2, "r1").await;
    a.hub().kick_user("room:id=r1", "alice").await.unwrap();
    let frame = next_frame(&mut listener.1).await;
    assert_eq!(
        frame,
        json!({ "kind": "unsubscribed", "topic": "room:id=r1", "reason": "kicked" })
    );
    assert_eq!(b.hub().subscriber_count("room:id=r1"), 0);
}

#[tokio::test]
async fn device_call_never_reaches_another_users_device() {
    let (a, b) = two_nodes();
    let (_device, mut device_out) = authed(&b, "alice/phone-1").await;
    let mut caller = authed(&a, "admin").await;
    locate_on(&caller, "mallory", "phone-1");
    assert_eq!(next_frame(&mut caller.1).await["error"]["code"], 404);
    assert!(device_out.try_recv().is_err(), "the device was called");
}

// ── review regressions ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_second_auth_frame_does_not_close_an_authenticated_session() {
    let (session, mut outgoing) = authed(&router(), "alice").await;
    session.handle(&json!({ "kind": "auth", "id": 2, "params": { "token": "alice" } }));
    assert_eq!(next_frame(&mut outgoing).await["error"]["code"], 409);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), session.closed())
            .await
            .is_err(),
        "an authenticated session was closed by a duplicate auth"
    );
}

#[tokio::test]
async fn concurrent_auth_frames_authenticate_once() {
    let router = router();
    let (session, mut outgoing) = open_session(&router, Req::default());
    session.handle(&json!({ "kind": "auth", "id": 1, "params": { "token": "alice" } }));
    session.handle(&json!({ "kind": "auth", "id": 2, "params": { "token": "bob" } }));
    let mut codes = [
        next_frame(&mut outgoing).await,
        next_frame(&mut outgoing).await,
    ];
    codes.sort_by_key(|frame| frame["id"].as_u64());
    assert_eq!(codes[0]["status"], "ok");
    assert_eq!(codes[1]["error"]["code"], 409);
    assert_eq!(router.hub().subscriber_count("user:id=alice"), 1);
    assert_eq!(router.hub().subscriber_count("user:id=bob"), 0);
}

#[tokio::test]
async fn another_user_cannot_claim_a_held_device() {
    let router = router();
    let _owner = authed(&router, "alice/phone-1").await;
    let (thief, mut outgoing) = open_session(&router, Req::default());
    thief.handle(&json!({ "kind": "auth", "id": 1, "params": { "token": "mallory/phone-1" } }));
    assert_eq!(next_frame(&mut outgoing).await["error"]["code"], 409);
    assert_eq!(
        closed_within(&thief, Duration::from_secs(1)).await,
        CloseReason::Unauthorized
    );
}

#[tokio::test]
async fn reconnecting_device_supersedes_the_old_session() {
    let router = router();
    let (old, _old_out) = authed(&router, "alice/phone-1").await;
    let _new = authed(&router, "alice/phone-1").await;
    assert_eq!(
        closed_within(&old, Duration::from_secs(1)).await,
        CloseReason::Superseded
    );
}

#[tokio::test]
async fn publish_with_unit_params_is_delivered() {
    let router = router();
    let (_session, mut outgoing) = authed(&router, "alice").await;
    router
        .hub()
        .publish::<Locate>("user:id=alice", &())
        .await
        .unwrap();
    assert_eq!(next_frame(&mut outgoing).await["params"], Value::Null);
}

#[tokio::test]
async fn subscribe_ack_always_precedes_publishes() {
    let router = router();
    let hub = router.hub().clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let publisher = tokio::spawn({
        let stop = Arc::clone(&stop);
        async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let text = Text { text: "x".into() };
                hub.publish::<Message>("room:id=r1", &text).await.unwrap();
                tokio::task::yield_now().await;
            }
        }
    });
    for round in 0..20 {
        let mut open = authed(&router, &format!("user{round}")).await;
        let first = subscribe(&mut open, 2, "r1").await;
        assert_eq!(first["kind"], "result", "a publish overtook the ack");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    publisher.await.unwrap();
}

#[tokio::test]
async fn flooding_frames_closes_the_session() {
    let config = SessionConfig {
        max_frames_per_second: 5,
        ..SessionConfig::default()
    };
    let router = router_on(Hub::default(), config);
    let (session, _outgoing) = authed(&router, "alice").await;
    for _ in 0..10 {
        session.handle(&json!({ "kind": "pong" }));
    }
    assert_eq!(
        closed_within(&session, Duration::from_secs(1)).await,
        CloseReason::RateLimited
    );
}

#[tokio::test]
async fn closing_a_session_fails_its_pending_client_calls() {
    let router = router();
    let (device, mut device_out) = authed(&router, "alice/phone-1").await;
    let hub = router.hub().clone();
    let waiting = tokio::spawn(async move {
        Cx::test(App, Req::default())
            .with_hub(hub)
            .client_call_device::<Locate>("alice", "phone-1", ())
            .await
    });
    assert_eq!(next_frame(&mut device_out).await["kind"], "client_call");
    drop(device);
    let error = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("pending call waited for its timeout")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, 410);
}

#[test]
#[should_panic(expected = "reserved")]
fn reserved_channel_names_are_refused() {
    #[derive(Model, Serialize, Deserialize)]
    struct Fake {
        id: String,
    }
    impl Channel for Fake {
        const NAME: &'static str = "user";
    }
    impl Authorize<App, Req> for Fake {
        async fn authorize(&self, _cx: &Cx<App, Req>) -> Result<bool, Error> {
            Ok(true)
        }
    }
    let _ = Router::<App, Req>::builder().ctx(App).channel::<Fake>();
}
