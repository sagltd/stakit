//! `Error::with_data`: an application error can carry a client-facing JSON
//! payload (e.g. `{ "resetsAt": … }` on a quota error) next to its message. It
//! rides in the unary envelope and in stream error frames, and is absent unless
//! set. Names are single words so the test holds with and without `camel`.

use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use stakit_model::Model;
use stakit_router::{Cx, Error, Frame, Router, action};

#[derive(Model, Serialize, Deserialize)]
struct Ask {
    detailed: bool,
}

struct App;

fn quota_error(detailed: bool) -> Error {
    let error = Error::coded(402, "quota_exceeded", "quota used up");
    if detailed {
        return error.with_data(json!({ "resetsAt": "2026-11-01T00:00:00Z" }));
    }
    error
}

#[action]
async fn ask(_cx: &Cx<App, ()>, params: Ask) -> Result<u32, Error> {
    Err(quota_error(params.detailed))
}

#[action(stream)]
fn probe(params: Ask) -> impl Stream<Item = Result<u32, Error>> + use<> {
    futures::stream::iter([Err(quota_error(params.detailed))])
}

fn router() -> Router<App, ()> {
    Router::builder()
        .ctx(App)
        .register(ask)
        .register_stream(probe)
        .build()
}

fn call(detailed: bool) -> Value {
    let out = block_on(router().on_request((), json!({ "ask": { "detailed": detailed } })));
    out["ask"].clone()
}

#[test]
fn unary_error_carries_its_data() {
    let env = call(true);
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["type"], "quota_exceeded");
    assert_eq!(env["error"]["message"], "quota used up");
    assert_eq!(env["error"]["data"]["resetsAt"], "2026-11-01T00:00:00Z");
}

#[test]
fn data_is_absent_unless_set() {
    let env = call(false);
    assert!(env["error"].get("data").is_none(), "{env}");
}

#[test]
fn stream_error_frame_carries_its_data() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream((), json!({ "probe": { "detailed": true } }))
            .collect(),
    );
    let json = serde_json::to_value(&frames).expect("frames serialize");
    assert_eq!(json[0]["type"], "error", "{json}");
    assert_eq!(json[0]["error"]["data"]["resetsAt"], "2026-11-01T00:00:00Z");
}

#[test]
fn typescript_envelope_declares_optional_data() {
    let ts = router().generate_ts();
    assert!(ts.contains("  data?: unknown;\n"), "{ts}");
}
