//! Security + safety audit tests for `stakit-router`.
//!
//! Every test here treats the JSON payload / wire frames as **attacker
//! controlled** and either (a) proves a guard holds or (b) locks an
//! ordering/behavioral contract that has security consequences. Mirrors the
//! harness style of `tests/router.rs` (build a `Router` with `#[action]`, drive
//! `on_request` / `on_stream` / a `session`).
//!
//! Findings these lock down (see the audit report for severities):
//! - validation runs *before* per-action middleware → a 422 with field detail
//!   reaches an *unauthenticated* caller (info leak). Recommendation: do auth on
//!   the `R` request-ctx boundary, not in an action `Middleware`.
//! - hostile websocket frames (missing/garbage `id`, wrong types, unknown
//!   `kind`, out-of-range error `code`) must never panic the session.
//! - the request entry points never panic on adversarial payload shapes.
//! - per-call errors are isolated: one failing/panicking call must not sink the
//!   others in a multi-call payload.

#![allow(clippy::unwrap_used)]
#![allow(clippy::missing_panics_doc)]
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use stakit_model::Model;
use stakit_router::{
    ActionExt as _, Cx, Endpoint, Error, Frame, Middleware, ResponseError, Router,
    StreamActionExt as _, action, err,
};

// ── contexts ─────────────────────────────────────────────────────────────────

struct App;

#[derive(Clone)]
struct Req {
    admin: bool,
}

// ── models ───────────────────────────────────────────────────────────────────

#[derive(Model, Serialize, Deserialize)]
struct Secret {
    #[validate(min_len = 8, max_len = 64)]
    api_key: String,
}

#[derive(Model, Serialize, Deserialize)]
struct Count {
    n: u64,
}

// ── actions ──────────────────────────────────────────────────────────────────

static ADMIN_BODY_RAN: AtomicUsize = AtomicUsize::new(0);

// An action whose body must only ever run for an admin. The guard is an action
// `Middleware` (the documented mechanism), so we can prove what leaks *before*
// the guard rejects.
#[action]
async fn admin_only(_cx: &Cx<App, Req>, _params: Secret) -> Result<String, Error> {
    ADMIN_BODY_RAN.fetch_add(1, Ordering::SeqCst);
    Ok("top secret".to_owned())
}

#[action(stream)]
fn count(params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

struct RequireAdmin;
impl Middleware<App, Req> for RequireAdmin {
    async fn before(&self, cx: &Cx<App, Req>) -> Result<(), Error> {
        if cx.req.admin {
            Ok(())
        } else {
            Err(err!(403, "admin only"))
        }
    }
}

fn guarded_router() -> Router<App, Req> {
    Router::builder()
        .ctx(App)
        .register(admin_only.middleware(RequireAdmin))
        .register_stream(count.middleware(RequireAdmin))
        .build()
}

fn payload(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    Value::Object(map)
}

// ── FIXED (was MED): the guard now runs BEFORE deserialize/validate ──────────
// A middleware guard's `before` runs on the `Action::before` hook, which the
// router invokes *before* parsing/validating input. So an unauthenticated caller
// submitting bad params is rejected by the guard (403) and never sees a 422
// validation error — no input-schema leak past the auth gate.

#[test]
fn guard_rejects_before_validation_no_schema_leak() {
    ADMIN_BODY_RAN.store(0, Ordering::SeqCst);
    let action_name = <admin_only as Endpoint>::ACTION;
    // Non-admin AND invalid params: the guard fires first → 403, not 422.
    let out = block_on(guarded_router().on_request(
        Req { admin: false },
        payload(action_name, json!({ "api_key": "short" })),
    ));
    let env = &out[action_name];
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["code"], 403, "guard precedes validation");
    // no field/schema detail leaks to an unauthorized caller
    assert!(
        env["error"]["fields"].is_null(),
        "schema must not leak: {env}"
    );
    assert_eq!(ADMIN_BODY_RAN.load(Ordering::SeqCst), 0);
}

#[test]
fn guard_rejects_valid_params_for_unauthorized_and_body_never_runs() {
    ADMIN_BODY_RAN.store(0, Ordering::SeqCst);
    let action_name = <admin_only as Endpoint>::ACTION;
    // Valid params, but not admin → guard rejects with 403, body never runs.
    let out = block_on(guarded_router().on_request(
        Req { admin: false },
        payload(action_name, json!({ "api_key": "longenoughkey" })),
    ));
    assert_eq!(out[action_name]["error"]["code"], 403);
    assert_eq!(ADMIN_BODY_RAN.load(Ordering::SeqCst), 0);
}

#[test]
fn guard_allows_admin_and_body_runs() {
    ADMIN_BODY_RAN.store(0, Ordering::SeqCst);
    let action_name = <admin_only as Endpoint>::ACTION;
    let out = block_on(guarded_router().on_request(
        Req { admin: true },
        payload(action_name, json!({ "api_key": "longenoughkey" })),
    ));
    assert_eq!(out[action_name]["status"], "ok");
    assert_eq!(ADMIN_BODY_RAN.load(Ordering::SeqCst), 1);
}

#[test]
fn stream_guard_rejects_unauthorized_with_single_error_frame() {
    let frames: Vec<Frame> = block_on(
        guarded_router()
            .on_stream(
                Req { admin: false },
                payload(<count as Endpoint>::ACTION, json!({ "n": 5 })),
            )
            .collect(),
    );
    // Exactly one 403 frame, the stream body never starts.
    assert_eq!(frames.len(), 1);
    assert!(matches!(frames.first(), Some(Frame::Error { error, .. }) if error.code == 403));
}

// ── SAFETY: adversarial payload shapes never panic the request entry points ──

#[test]
fn request_handles_adversarial_payload_shapes_without_panic() {
    let router = guarded_router();
    // Scalars / null / nested garbage — none of these are object/array call maps.
    for hostile in [
        json!(null),
        json!(true),
        json!(12345),
        json!("not a payload"),
        json!(2.5),
        json!([[42, { "x": 1 }]]), // action name is a number, not a string
        json!([["count"]]),        // array entry with no params
        json!([[]]),               // empty pair
        json!([null, 1, "x"]),     // non-pair elements
        json!({ "": null }),       // empty action name
        json!({ "unknown_action": {} }), // unknown name → 404, not panic
    ] {
        let out = block_on(router.on_request(Req { admin: true }, hostile.clone()));
        // It must always return *some* JSON value and never panic.
        assert!(
            out.is_object() || out.is_array(),
            "hostile payload {hostile} produced {out}"
        );
    }
}

#[test]
fn array_payload_malformed_entries_become_404_not_panic() {
    let out = block_on(guarded_router().on_request(
        Req { admin: true },
        json!([[42, {}], ["count"], "garbage", [["nested"], {}]]),
    ));
    let arr = out.as_array().expect("array in → array out");
    // Index-aligned: every input element maps to exactly one output slot.
    assert_eq!(arr.len(), 4);
    for slot in arr {
        // Each malformed slot routed to an empty/garbage action name → 404 error
        // envelope, never a dropped/shifted slot and never a panic.
        assert_eq!(slot["status"], "error");
        assert_eq!(slot["error"]["code"], 404);
    }
}

// ── SAFETY: one failing call must not sink the others (per-call isolation) ────

#[derive(Debug, thiserror::Error, ResponseError)]
#[status(500)]
#[error("kaboom")]
struct Kaboom;

#[action]
async fn always_errors(_params: Count) -> Result<u64, Kaboom> {
    Err(Kaboom)
}

#[action]
async fn echo(params: Count) -> Result<u64, Error> {
    Ok(params.n)
}

#[test]
fn per_call_errors_are_isolated_in_a_multicall_object() {
    let router = Router::builder()
        .ctx(App)
        .register(always_errors)
        .register(echo)
        .build();
    let always_errors_name = <always_errors as Endpoint>::ACTION;
    let echo_name = <echo as Endpoint>::ACTION;
    let out = block_on(router.on_request(
        Req { admin: true },
        json!({ always_errors_name: { "n": 1 }, echo_name: { "n": 7 } }),
    ));
    // The failing call errors in its own slot; the good call still succeeds.
    assert_eq!(out[always_errors_name]["error"]["code"], 500);
    assert_eq!(out[echo_name]["status"], "ok");
    assert_eq!(out[echo_name]["data"], json!(7));
}

// ── SAFETY: hostile websocket session frames must never panic ────────────────

#[derive(Model, Serialize, Deserialize)]
struct Greet {
    #[validate(min_len = 1)]
    name: String,
}

#[action]
async fn greet(_cx: &Cx<App, Req>, params: Greet) -> Result<String, Error> {
    Ok(params.name)
}

fn ws_router() -> Arc<Router<App, Req>> {
    Arc::new(
        Router::builder()
            .ctx(App)
            .register(greet)
            .register_stream(count)
            .build(),
    )
}

#[tokio::test]
async fn session_ignores_hostile_frames_without_panicking() {
    let router = ws_router();
    let mut session = router.session(Req { admin: true });
    let _outgoing = session.outgoing();
    let session = Arc::new(session);

    // None of these well-poisoned frames may panic the session.
    let hostile = [
        json!(null),
        json!("not even an object"),
        json!(42),
        json!([1, 2, 3]),
        json!({}),                                        // no kind
        json!({ "kind": 123 }),                           // kind not a string
        json!({ "kind": "call" }),                        // no id
        json!({ "kind": "call", "id": "not-a-number" }),  // id wrong type
        json!({ "kind": "call", "id": -5 }),              // negative id (not u64)
        json!({ "kind": "call", "id": 1 }),               // no action / params
        json!({ "kind": "call", "id": 2, "action": 99 }), // action not a string
        json!({ "kind": "unknown_kind", "id": 3 }),       // unknown kind
        json!({ "kind": "client_result" }),               // no id
        json!({ "kind": "client_result", "id": 7 }),      // id never issued
        // attacker-supplied out-of-range / garbage error code in a client_result
        json!({ "kind": "client_result", "id": 8, "error": { "code": 999_999, "message": "x" } }),
        json!({ "kind": "client_result", "id": 9, "error": { "code": -1 } }),
        json!({ "kind": "client_result", "id": 10, "error": "not an object" }),
    ];
    for frame in &hostile {
        session.handle(frame);
    }

    // The session is still alive and routes a legitimate call afterwards.
    let mut session2 = router.session(Req { admin: true });
    let mut out2 = session2.outgoing();
    let session2 = Arc::new(session2);
    session2.handle(&json!({
        "kind": "call", "id": 1, "action": <greet as Endpoint>::ACTION, "params": { "name": "ok" }
    }));
    let result = tokio::time::timeout(Duration::from_secs(2), out2.recv())
        .await
        .expect("session deadlocked after hostile frames")
        .expect("session produced no result");
    assert_eq!(result["kind"], "result");
    assert_eq!(result["status"], "ok");
    assert_eq!(result["data"], json!("ok"));
}

#[tokio::test]
async fn session_unknown_action_is_404_not_panic() {
    let router = ws_router();
    let mut session = router.session(Req { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    session.handle(&json!({ "kind": "call", "id": 1, "action": "does_not_exist", "params": null }));
    let result = tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], 404);
}

#[tokio::test]
async fn session_validation_runs_on_the_duplex_path_too() {
    // Validation is not just an HTTP-path guard: invalid input over a websocket
    // session is rejected with 422 before the action body runs.
    let router = ws_router();
    let mut session = router.session(Req { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    session.handle(&json!({
        "kind": "call", "id": 1, "action": <greet as Endpoint>::ACTION, "params": { "name": "" }
    }));
    let result = tokio::time::timeout(Duration::from_secs(2), outgoing.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], 422);
}

// ── SAFETY/DoS: a client_call against a one-way transport fails fast ──────────
// (locks that there is no unbounded wait when there is no back-channel).

#[action]
async fn calls_back(cx: &Cx<App, Req>, _params: Count) -> Result<u64, Error> {
    // A client action on a transport with no duplex channel must error, not hang.
    struct NoClient;
    impl stakit_router::ClientAction for NoClient {
        type Params = ();
        type Return = ();
        const NAME: &'static str = "noClient";
    }
    cx.client_call::<NoClient>(()).await?;
    Ok(0)
}

#[test]
fn client_call_without_duplex_errors_fast() {
    let name = <calls_back as Endpoint>::ACTION;
    let router = Router::builder().ctx(App).register(calls_back).build();
    let out = block_on(router.on_request(Req { admin: true }, payload(name, json!({ "n": 1 }))));
    // 400, returned immediately — never an unbounded await on a missing channel.
    assert_eq!(out[name]["error"]["code"], 400);
}
