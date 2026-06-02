//! Crash safety: a panicking action (or guard, or stream item) must never crash
//! the app or the connection — it is caught and surfaced as a generic `500`
//! error, the panic's real text kept server-side (never leaked to the client).

use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use stakit_model::Model;
use stakit_router::{Cx, Error, Frame, Middleware, Router, StreamActionExt as _, action};

#[derive(Model, Serialize, Deserialize)]
struct In {
    n: u32,
}

// --- unary actions ---

#[action]
fn boom() -> Result<String, Error> {
    panic!("secret connection string leaked in panic");
}

#[action]
fn ping() -> Result<String, Error> {
    Ok("pong".to_owned())
}

// --- stream actions ---

#[action(stream)]
fn boom_mid(params: In) -> impl futures::Stream<Item = Result<u32, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            assert!(i != 1, "kaboom mid-stream");
            yield Ok(i);
        }
    }
}

#[action(stream)]
fn boom_first(_params: In) -> impl futures::Stream<Item = Result<u32, Error>> {
    async_stream::stream! {
        panic!("kaboom on first poll");
        #[allow(unreachable_code)]
        {
            yield Ok(0u32);
        }
    }
}

#[action(stream)]
fn good(params: In) -> impl futures::Stream<Item = Result<u32, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

// A teardown hook that panics — exercises the `after` catch in the stream path.
struct PanicAfter;
impl Middleware<(), ()> for PanicAfter {
    async fn after(&self, _cx: &Cx<(), ()>) {
        panic!("teardown boom");
    }
}

fn router() -> Router<(), ()> {
    Router::builder()
        .ctx(())
        .register(boom)
        .register(ping)
        .register_stream(boom_mid)
        .register_stream(boom_first)
        .register_stream(good.middleware(PanicAfter))
        .build()
}

fn payload(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    Value::Object(map)
}

#[test]
fn panicking_action_becomes_500_not_a_crash() {
    let out = block_on(router().on_request((), payload("boom", json!(null))));
    let env = &out["boom"];
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["code"], 500);
    // Generic client-facing message — the panic text must NOT leak.
    assert_eq!(env["error"]["message"], "internal server error");
    assert!(
        !env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("secret connection string")
    );
}

#[test]
fn app_survives_a_panic_and_keeps_serving() {
    let router = router();
    // First call panics...
    let _ = block_on(router.on_request((), payload("boom", json!(null))));
    // ...the router is unharmed and still dispatches the next call.
    let out = block_on(router.on_request((), payload("ping", json!(null))));
    assert_eq!(out["ping"]["status"], "ok");
    assert_eq!(out["ping"]["data"], json!("pong"));
}

#[test]
fn panic_mid_stream_ends_with_error_frame_after_good_items() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream((), payload("boom_mid", json!({ "n": 5 })))
            .collect(),
    );
    // Item 0 streamed fine, then the panic terminated the substream with an error
    // frame — no `end`, and no items past the panic.
    let mut iter = frames.iter();
    assert!(matches!(iter.next(), Some(Frame::Next { data, .. }) if *data == json!(0)));
    match iter.next() {
        Some(Frame::Error { error, .. }) => {
            assert_eq!(error.code, 500);
            assert_eq!(error.message, "internal server error");
        }
        other => panic!("expected error frame after first item, got {other:?}"),
    }
    assert!(iter.next().is_none(), "no frames after the error frame");
}

#[test]
fn panic_on_first_stream_poll_is_an_error_frame() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream((), payload("boom_first", json!({ "n": 3 })))
            .collect(),
    );
    assert_eq!(frames.len(), 1);
    assert!(matches!(&frames[0], Frame::Error { error, .. } if error.code == 500));
}

#[test]
fn panicking_after_hook_yields_error_frame_after_all_items() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream((), payload("good", json!({ "n": 2 })))
            .collect(),
    );
    // All items delivered, then the teardown panic becomes a final error frame —
    // no `end`, and the process survives.
    assert!(matches!(&frames[0], Frame::Next { data, .. } if *data == json!(0)));
    assert!(matches!(&frames[1], Frame::Next { data, .. } if *data == json!(1)));
    match &frames[2] {
        Frame::Error { error, .. } => assert_eq!(error.code, 500),
        other => panic!("expected error frame from panicking teardown, got {other:?}"),
    }
    assert_eq!(frames.len(), 3);
}

#[test]
fn stream_panic_does_not_crash_subsequent_requests() {
    let router = router();
    let _: Vec<Frame> = block_on(
        router
            .on_stream((), payload("boom_first", json!({ "n": 1 })))
            .collect(),
    );
    let out = block_on(router.on_request((), payload("ping", json!(null))));
    assert_eq!(out["ping"]["status"], "ok");
}
