//! End-to-end tests for `#[action]` + `Router`, covering every signature shape,
//! validation, action-to-action calls, payload routing (object + ordered array),
//! multi-action requests/streams, and TypeScript generation.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

use stakit_model::Model;
use stakit_router::{
    Action, ActionExt as _, ClientAction, Cx, Endpoint, Error, Frame, Middleware, ResponseError,
    Router, StreamActionExt as _, action, err,
};

// --- contexts ---
struct App {
    greeting: String,
}
#[derive(Clone)]
struct Req {
    admin: bool,
}

// --- models ---
#[derive(Model, Serialize, Deserialize)]
struct Greet {
    #[validate(min_len = 1, max_len = 20)]
    name: String,
}

#[derive(Model, Serialize)]
struct Greeting {
    message: String,
}

// --- actions: every signature shape ---

#[action]
async fn greet(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: format!("{}, {}!", cx.app.greeting, params.name),
    })
}

#[action]
fn ping() -> Result<String, Error> {
    Ok("pong".to_owned())
}

#[action]
async fn whoami(cx: &Cx<App, Req>) -> Result<bool, Error> {
    Ok(cx.req.admin)
}

#[action]
async fn greet_twice(cx: &Cx<App, Req>, params: Greet) -> Result<String, Error> {
    // action-to-action call, typed, still validated
    let g = cx.call(greet, params).await?;
    Ok(format!("{} {}", g.message, g.message))
}

// --- custom application error: returned straight from an action ---
// A 5xx error: thiserror gives `Display`, `ResponseError` gives the status. The
// `From` genericizes the client message and keeps the real text in `detail`.
#[derive(Debug, thiserror::Error, ResponseError)]
#[status(500)]
#[error("{0}")]
struct AppError(&'static str);

#[action]
async fn maybe_fail(params: Greet) -> Result<Greeting, AppError> {
    if params.name == "boom" {
        return Err(AppError("kaboom")); // 5xx -> generic message, detail kept
    }
    Ok(Greeting {
        message: params.name,
    })
}

#[action]
async fn find(params: Greet) -> Result<Greeting, Error> {
    if params.name == "missing" {
        return Err(err!(404, "not found")); // explicit code via err!
    }
    Ok(Greeting {
        message: params.name,
    })
}

// --- streaming action (no ctx needed) ---
#[derive(Model, Serialize, Deserialize)]
struct Count {
    n: u64,
}

#[action(stream)]
fn count(params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

// A streaming action that calls a *client* action on every iteration — proves
// stream + `client_call` works over a duplex (websocket) session.
#[action(stream)]
fn progress<'a>(
    cx: &'a Cx<App, Req>,
    params: Count,
) -> impl futures::Stream<Item = Result<u64, Error>> + 'a {
    async_stream::stream! {
        for i in 0..params.n {
            match cx
                .client_call::<ShowToast>(Toast { text: format!("step {i}") })
                .await
            {
                Ok(_ack) => yield Ok(i),
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
}

// --- thiserror + ResponseError app error works out of the box ---
#[derive(Debug, thiserror::Error, ResponseError)]
enum TodoError {
    #[status(500)]
    #[error("database is down")]
    Db,
}

#[action]
async fn risky(_params: Greet) -> Result<Greeting, TodoError> {
    Err(TodoError::Db) // thiserror Display + ResponseError status -> Into<Error> (500)
}

// --- client action (server -> client) for duplex ---
#[derive(Model, Serialize, Deserialize)]
struct Toast {
    text: String,
}

struct ShowToast;
impl ClientAction for ShowToast {
    type Params = Toast;
    type Return = String;
    const NAME: &'static str = "showToast";
}

#[action]
async fn notify_user(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    let ack: String = cx
        .client_call::<ShowToast>(Toast { text: params.name })
        .await?;
    Ok(Greeting { message: ack })
}

fn router() -> Router<App, Req> {
    Router::builder()
        .ctx(App {
            greeting: "Hello".to_owned(),
        })
        .register(greet)
        .register(ping)
        .register(whoami)
        .register(greet_twice)
        .register(maybe_fail)
        .register(find)
        .register(risky)
        .register(notify_user)
        .register_stream(count)
        .register_stream(progress)
        .client_action::<ShowToast>()
        .build()
}

/// Builds a single-call object payload `{ action: params }`.
fn payload(action: &str, params: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(action.to_owned(), params);
    Value::Object(map)
}

/// Calls one action and returns its envelope (`{status, data | error}`).
fn call(router: &Router<App, Req>, admin: bool, action: &str, params: Value) -> Value {
    block_on(router.on_request(Req { admin }, payload(action, params)))[action].clone()
}

#[test]
fn dispatches_and_runs_with_ctx() {
    let env = call(&router(), true, "greet", json!({"name": "bob"}));
    assert_eq!(env["status"], "ok");
    assert_eq!(env["data"], json!({"message": "Hello, bob!"}));
}

#[test]
fn param_less_and_ctx_less_action() {
    let env = call(&router(), false, "ping", json!(null));
    assert_eq!(env["status"], "ok");
    assert_eq!(env["data"], json!("pong"));
}

#[test]
fn ctx_only_action_reads_request_ctx() {
    let env = call(&router(), true, "whoami", json!(null));
    assert_eq!(env["data"], json!(true));
}

#[test]
fn invalid_params_yield_validation_error() {
    let env = call(&router(), true, "greet", json!({"name": ""}));
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["code"], 422);
    assert!(env["error"]["fields"]["name"].is_array());
}

#[test]
fn unknown_action_is_404() {
    let env = call(&router(), true, "nope", json!(null));
    assert_eq!(env["error"]["code"], 404);
}

#[test]
fn malformed_params_yield_decode_error() {
    // `name` must be a string — a number fails to deserialize → 400.
    let env = call(&router(), true, "greet", json!({"name": 123}));
    assert_eq!(env["status"], "error");
    assert_eq!(env["error"]["code"], 400);
}

#[test]
fn action_to_action_call() {
    let env = call(
        &router(),
        true,
        <greet_twice as Endpoint>::ACTION,
        json!({"name": "sam"}),
    );
    assert_eq!(env["data"], json!("Hello, sam! Hello, sam!"));
}

#[test]
fn object_payload_routes_multiple_actions() {
    let greet_name = <greet as Endpoint>::ACTION;
    let ping_name = <ping as Endpoint>::ACTION;
    let find_name = <find as Endpoint>::ACTION;
    let out = block_on(router().on_request(
        Req { admin: true },
        json!({ greet_name: { "name": "sam" }, ping_name: null, find_name: { "name": "missing" } }),
    ));
    assert_eq!(out[greet_name]["data"]["message"], "Hello, sam!");
    assert_eq!(out[ping_name]["data"], json!("pong"));
    assert_eq!(out[find_name]["error"]["code"], 404);
}

#[test]
fn array_payload_preserves_order_and_allows_duplicates() {
    let out = block_on(router().on_request(
        Req { admin: true },
        json!([["greet", { "name": "a" }], ["greet", { "name": "b" }], ["ping", null]]),
    ));
    let array = out.as_array().expect("array response for array payload");
    assert_eq!(array.len(), 3);
    assert_eq!(array[0]["data"]["message"], "Hello, a!");
    assert_eq!(array[1]["data"]["message"], "Hello, b!");
    assert_eq!(array[2]["data"], json!("pong"));
}

#[test]
fn array_payload_keeps_index_alignment_on_malformed_entries() {
    let out = block_on(router().on_request(
        Req { admin: true },
        json!([["greet", { "name": "a" }], "garbage", ["ping", null]]),
    ));
    let array = out.as_array().expect("array response");
    // every input element maps to exactly one output slot (no index shift)
    assert_eq!(array.len(), 3);
    assert_eq!(array[0]["data"]["message"], "Hello, a!");
    assert_eq!(array[1]["status"], "error"); // malformed → error envelope, slot kept
    assert_eq!(array[2]["data"], json!("pong"));
}

// Mirrors how you'd wire this into axum: the router lives in shared state; one
// HTTP handler extracts the request ctx + decoded payload, calls `on_request`,
// and serializes the response. The action name is *in the payload*, not the URL.
#[derive(Clone)]
struct AppState {
    router: Arc<Router<App, Req>>,
}

fn http_handler(state: &AppState, headers: &[(&str, &str)], body: Value) -> (u16, Value) {
    let admin = headers.iter().any(|(k, v)| *k == "x-admin" && *v == "true");
    let response = block_on(state.router.on_request(Req { admin }, body));
    // HTTP status is always 200; per-action codes live in each envelope.
    (200, response)
}

#[test]
fn axum_style_single_handler_routes_everything() {
    let state = AppState {
        router: Arc::new(router()),
    };

    let (code, body) = http_handler(
        &state,
        &[("x-admin", "true")],
        json!({ "greet": { "name": "sam" } }),
    );
    assert_eq!(code, 200);
    assert_eq!(body["greet"]["status"], "ok");
    assert_eq!(body["greet"]["data"]["message"], "Hello, sam!");

    let (_code, body) = http_handler(&state, &[], json!({ "greet": { "name": "" } }));
    assert_eq!(body["greet"]["status"], "error");
    assert_eq!(body["greet"]["error"]["code"], 422);
}

#[test]
fn custom_app_error_is_generic_500_with_detail_kept_server_side() {
    let env = call(
        &router(),
        true,
        <maybe_fail as Endpoint>::ACTION,
        json!({"name": "boom"}),
    );
    assert_eq!(env["error"]["code"], 500);
    // client sees a generic message — the internal text is NOT leaked
    assert_eq!(env["error"]["message"], "internal server error");
    // ...it's kept server-side (for logging) in `Error::detail`
    let e: Error = AppError("kaboom").into();
    assert_eq!(e.detail(), Some("kaboom"));
}

#[test]
fn explicit_error_code_via_macro() {
    let env = call(&router(), true, "find", json!({"name": "missing"}));
    assert_eq!(env["error"]["code"], 404);
}

#[test]
fn streaming_action_yields_frames() {
    let count_name = <count as Endpoint>::ACTION;
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream(Req { admin: true }, payload(count_name, json!({"n": 3})))
            .collect(),
    );
    assert_eq!(frames.len(), 4); // 3 items + End
    match &frames[0] {
        Frame::Next {
            index,
            action,
            data,
        } => {
            assert_eq!(*index, 0);
            assert_eq!(action, count_name);
            assert_eq!(data, &json!(0));
        }
        other => panic!("expected Next, got {other:?}"),
    }
    assert!(matches!(&frames[3], Frame::End { action, .. } if action == count_name));
}

#[test]
fn streaming_unknown_action_errors() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream(Req { admin: true }, payload("nope", json!(null)))
            .collect(),
    );
    assert!(matches!(frames.first(), Some(Frame::Error { error, .. }) if error.code == 404));
}

#[test]
fn stream_payload_runs_multiple_actions() {
    let count_name = <count as Endpoint>::ACTION;
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream(
                Req { admin: true },
                json!([[count_name, { "n": 1 }], [count_name, { "n": 1 }]]),
            )
            .collect(),
    );
    // each call: 1 Next + 1 End = 4 frames; both indices present.
    assert_eq!(frames.len(), 4);
    let indices: Vec<usize> = frames
        .iter()
        .map(|frame| match frame {
            Frame::Next { index, .. } | Frame::End { index, .. } | Frame::Error { index, .. } => {
                *index
            }
        })
        .collect();
    assert!(indices.contains(&0) && indices.contains(&1));
}

#[test]
fn thiserror_app_error_works_out_of_the_box() {
    let env = call(&router(), true, "risky", json!({"name": "x"}));
    assert_eq!(env["error"]["code"], 500);
    // generic to the client; thiserror's text stays server-side in `detail`
    assert_eq!(env["error"]["message"], "internal server error");
    let e: Error = TodoError::Db.into();
    assert_eq!(e.detail(), Some("database is down"));
}

#[tokio::test]
async fn duplex_client_call_roundtrip() {
    let router = Arc::new(router());
    let mut session = router.session(Req { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    // Client invokes the server action `notify_user` (camelCase under `camel` feature).
    session.handle(
        &json!({ "kind": "call", "id": 1, "action": <notify_user as Endpoint>::ACTION, "params": { "name": "bob" } }),
    );

    // The action calls back into the client (`client_call`); we receive that frame.
    let call = outgoing.recv().await.unwrap();
    assert_eq!(call["kind"], "client_call");
    assert_eq!(call["name"], "showToast");
    assert_eq!(call["params"]["text"], "bob");
    let call_id = call["id"].as_u64().unwrap();

    // Client replies; the suspended action resumes.
    session.handle(&json!({ "kind": "client_result", "id": call_id, "data": "delivered" }));

    // The action's result comes back tagged with the original call id.
    let result = outgoing.recv().await.unwrap();
    assert_eq!(result["kind"], "result");
    assert_eq!(result["id"], 1);
    assert_eq!(result["status"], "ok");
    assert_eq!(result["data"]["message"], "delivered");
}

#[tokio::test]
async fn duplex_streams_over_websocket() {
    let router = Arc::new(router());
    let mut session = router.session(Req { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    session.handle(&json!({ "kind": "call", "id": 7, "action": <count as Endpoint>::ACTION, "params": { "n": 2 } }));

    let f0 = outgoing.recv().await.unwrap();
    assert_eq!(f0["kind"], "result");
    assert_eq!(f0["data"], json!(0));
    let _f1 = outgoing.recv().await.unwrap();
    let end = outgoing.recv().await.unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["id"], 7);
}

#[tokio::test]
async fn duplex_stream_action_can_call_client_actions() {
    let router = Arc::new(router());
    let mut session = router.session(Req { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    // A streaming action `progress` that calls back to the client each iteration.
    session.handle(&json!({ "kind": "call", "id": 1, "action": <progress as Endpoint>::ACTION, "params": { "n": 2 } }));

    for expected in 0..2u64 {
        // server → client call
        let call = outgoing.recv().await.unwrap();
        assert_eq!(call["kind"], "client_call");
        assert_eq!(call["name"], "showToast");
        let call_id = call["id"].as_u64().unwrap();
        // client replies; the suspended stream resumes and yields its item
        session.handle(&json!({ "kind": "client_result", "id": call_id, "data": "ok" }));
        let item = outgoing.recv().await.unwrap();
        assert_eq!(item["kind"], "result");
        assert_eq!(item["data"], json!(expected));
    }

    let end = outgoing.recv().await.unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["id"], 1);
}

#[test]
fn http_stream_client_call_errors_without_duplex() {
    // Over a one-way HTTP stream there is no back-channel, so `client_call`
    // fails fast with a clear 400 instead of hanging.
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream(
                Req { admin: true },
                payload(<progress as Endpoint>::ACTION, json!({ "n": 1 })),
            )
            .collect(),
    );
    assert!(
        matches!(frames.first(), Some(Frame::Error { error, .. }) if error.code == 400),
        "expected a 400 error frame, got {frames:?}"
    );
}

#[tokio::test]
async fn client_call_times_out_when_unanswered() {
    // A short, configurable timeout: a silent client must not pin the suspended
    // action (and its memory) forever — it fails with 504.
    let router = Arc::new(
        Router::builder()
            .ctx(App {
                greeting: "Hello".to_owned(),
            })
            .register(notify_user)
            .client_action::<ShowToast>()
            .client_call_timeout(Duration::from_millis(50))
            .build(),
    );
    let mut session = router.session(Req { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    session.handle(
        &json!({ "kind": "call", "id": 1, "action": <notify_user as Endpoint>::ACTION, "params": { "name": "x" } }),
    );

    // The server→client call goes out...
    let call = outgoing.recv().await.unwrap();
    assert_eq!(call["kind"], "client_call");

    // ...we never reply. The action's `client_call` times out → 504 result.
    let result = outgoing.recv().await.unwrap();
    assert_eq!(result["kind"], "result");
    assert_eq!(result["id"], 1);
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], 504);
}

// ── unit-testing an action with no server (Cx::test) ─────────────────────────

#[tokio::test]
async fn unit_test_action_with_test_cx() {
    // No router, no HTTP — just a context + the action.
    let cx = Cx::test(
        App {
            greeting: "Hello".to_owned(),
        },
        Req { admin: true },
    );
    let out = greet
        .run(
            &cx,
            Greet {
                name: "bob".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(out.message, "Hello, bob!");

    // action-to-action calls work the same way
    let twice = cx
        .call(
            greet_twice,
            Greet {
                name: "sam".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(twice, "Hello, sam! Hello, sam!");
}

#[tokio::test]
async fn unit_test_action_with_mocked_client_call() {
    // Stub the server→client call so an action that uses `client_call` is
    // testable without a websocket.
    let cx = Cx::test(
        App {
            greeting: "Hello".to_owned(),
        },
        Req { admin: true },
    )
    .with_client(|name, _params| {
        assert_eq!(name, "showToast");
        Ok(json!("delivered"))
    });
    let out = notify_user
        .run(
            &cx,
            Greet {
                name: "x".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(out.message, "delivered");
}

// ── middleware: JWT-style auth guard (validate token → inject user → gate) ───

#[derive(Clone, Default)]
struct AuthReq {
    bearer: Option<String>,
    user: OnceLock<String>, // filled by the guard, read by the action
}

struct JwtAuth;
impl Middleware<App, AuthReq> for JwtAuth {
    // plain `async fn`, no Box. Validates the token and injects the user; the
    // action body is never reached if this returns `Err`.
    async fn before(&self, cx: &Cx<App, AuthReq>) -> Result<(), Error> {
        let token = cx
            .req
            .bearer
            .as_deref()
            .ok_or_else(|| err!(401, "missing bearer token"))?;
        // (a real impl verifies a JWT signature/claims here)
        let user = token
            .strip_prefix("valid:")
            .ok_or_else(|| err!(401, "invalid token"))?;
        let _ = cx.req.user.set(user.to_owned()); // OnceLock = interior mutability
        Ok(())
    }
}

#[action]
async fn whoami_authed(cx: &Cx<App, AuthReq>) -> Result<String, Error> {
    // reads the user the guard injected
    cx.req
        .user
        .get()
        .cloned()
        .ok_or_else(|| err!(401, "unauthenticated"))
}

fn auth_router() -> Router<App, AuthReq> {
    Router::builder()
        .ctx(App {
            greeting: "Hi".to_owned(),
        })
        .register(whoami_authed.middleware(JwtAuth))
        .build()
}

#[test]
fn middleware_rejects_before_reaching_action() {
    let name = <whoami_authed as Endpoint>::ACTION;
    // no token → 401, action body never runs
    let denied = block_on(auth_router().on_request(AuthReq::default(), payload(name, json!(null))));
    assert_eq!(denied[name]["error"]["code"], 401);
}

static ACTION_CALLS: AtomicUsize = AtomicUsize::new(0);

#[action]
async fn counted(_cx: &Cx<App, AuthReq>) -> Result<(), Error> {
    ACTION_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

struct Deny;
impl Middleware<App, AuthReq> for Deny {
    async fn before(&self, _cx: &Cx<App, AuthReq>) -> Result<(), Error> {
        Err(err!(403, "denied"))
    }
}

#[test]
fn before_error_never_calls_the_action() {
    ACTION_CALLS.store(0, Ordering::SeqCst);
    let counted_name = <counted as Endpoint>::ACTION;
    let router = Router::builder()
        .ctx(App {
            greeting: "Hi".to_owned(),
        })
        .register(counted.middleware(Deny))
        .build();

    let out = block_on(router.on_request(AuthReq::default(), payload(counted_name, json!(null))));
    assert_eq!(out[counted_name]["error"]["code"], 403);
    // the action body never ran
    assert_eq!(ACTION_CALLS.load(Ordering::SeqCst), 0);
}

#[test]
fn middleware_passes_and_injects_user() {
    let name = <whoami_authed as Endpoint>::ACTION;
    let req = AuthReq {
        bearer: Some("valid:alice".to_owned()),
        user: OnceLock::new(),
    };
    let ok = block_on(auth_router().on_request(req, payload(name, json!(null))));
    assert_eq!(ok[name]["status"], "ok");
    assert_eq!(ok[name]["data"], json!("alice")); // injected by the guard
}

static AFTER_RAN: AtomicUsize = AtomicUsize::new(0);

struct Audit;
impl Middleware<App, AuthReq> for Audit {
    async fn after(&self, _cx: &Cx<App, AuthReq>) {
        AFTER_RAN.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn middleware_after_runs_once_the_action_completes() {
    ACTION_CALLS.store(0, Ordering::SeqCst);
    AFTER_RAN.store(0, Ordering::SeqCst);
    let router = Router::builder()
        .ctx(App {
            greeting: "Hi".to_owned(),
        })
        .register(counted.middleware(Audit))
        .build();

    let counted_name = <counted as Endpoint>::ACTION;
    block_on(router.on_request(AuthReq::default(), payload(counted_name, json!(null))));
    assert_eq!(ACTION_CALLS.load(Ordering::SeqCst), 1); // action ran
    assert_eq!(AFTER_RAN.load(Ordering::SeqCst), 1); // ...then `after`
}

// guard on a *streaming* action (exercises `StreamGuarded`).
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

#[test]
fn stream_middleware_guards_the_stream() {
    let router = Router::builder()
        .ctx(App {
            greeting: "Hi".to_owned(),
        })
        .register_stream(count.middleware(RequireAdmin))
        .build();

    let count_name = <count as Endpoint>::ACTION;
    // denied → a single 403 error frame, the stream body never starts
    let denied: Vec<Frame> = block_on(
        router
            .on_stream(Req { admin: false }, payload(count_name, json!({"n": 3})))
            .collect(),
    );
    assert!(matches!(denied.first(), Some(Frame::Error { error, .. }) if error.code == 403));
    assert_eq!(denied.len(), 1);

    // allowed → 2 items + End
    let allowed: Vec<Frame> = block_on(
        router
            .on_stream(Req { admin: true }, payload(count_name, json!({"n": 2})))
            .collect(),
    );
    assert_eq!(allowed.len(), 3);
    assert!(matches!(allowed.last(), Some(Frame::End { .. })));
}

// ── generic struct / enum as an action return ───────────────────────────────

#[derive(Model, Serialize, Deserialize)]
struct Message<T> {
    is_success: bool,
    data: T,
}

#[derive(Model, Serialize, Deserialize)]
struct User {
    id: u64,
    name: String,
}

#[derive(Model, Serialize, Deserialize)]
enum ApiResult<T> {
    Found(T),
    Missing,
}

#[action]
async fn login(_params: Greet) -> Result<Message<User>, Error> {
    Ok(Message {
        is_success: true,
        data: User {
            id: 1,
            name: "alice".to_owned(),
        },
    })
}

#[action]
async fn lookup(_params: Greet) -> Result<ApiResult<User>, Error> {
    Ok(ApiResult::Found(User {
        id: 2,
        name: "bob".to_owned(),
    }))
}

fn generic_router() -> Router<App, Req> {
    Router::builder()
        .ctx(App {
            greeting: "Hi".to_owned(),
        })
        .register(login)
        .register(lookup)
        .build()
}

#[test]
fn action_returns_generic_struct() {
    let out = block_on(
        generic_router().on_request(Req { admin: true }, payload("login", json!({"name": "x"}))),
    );
    assert_eq!(out["login"]["status"], "ok");
    assert_eq!(out["login"]["data"]["is_success"], true);
    assert_eq!(out["login"]["data"]["data"]["name"], "alice"); // nested generic value
}

#[test]
fn action_returns_generic_enum() {
    let out = block_on(
        generic_router().on_request(Req { admin: true }, payload("lookup", json!({"name": "x"}))),
    );
    // serde externally-tagged: ApiResult::Found(User) -> { "Found": {…} }
    assert_eq!(out["lookup"]["data"]["Found"]["name"], "bob");
}

#[test]
fn generic_types_generate_valid_typescript() {
    let ts = generic_router().generate_ts();
    // the monomorphized generic interface + the nested type it references
    assert!(ts.contains("export interface MessageUser {"), "{ts}");
    assert!(ts.contains("export interface User {"), "{ts}");
    assert!(ts.contains("data: User"), "{ts}");
    // result map references the monomorphized name
    assert!(ts.contains("login: MessageUser;"), "{ts}");
    assert!(ts.contains("lookup: ApiResultUser;"), "{ts}");
    // never a declaration embedded inline (the old nested-type bug)
    assert!(!ts.contains(": export "), "{ts}");
    assert!(!ts.contains("<export "), "{ts}");
}

#[test]
fn generates_typescript() {
    let ts = router().generate_ts();
    // model declarations
    assert!(ts.contains("export interface Greet {"), "{ts}");
    assert!(ts.contains("export interface Greeting {"), "{ts}");
    assert!(ts.contains("export interface Toast {"), "{ts}");
    assert!(ts.contains("message: string"), "{ts}");
    // typed maps — action keys use the stable name (camelCase under `camel` feature)
    assert!(ts.contains("export interface ActionParameters {"), "{ts}");
    let greet_key = format!("{}: Greet;", <greet as Endpoint>::ACTION);
    assert!(
        ts.contains(&greet_key),
        "expected `{greet_key}` in TS: {ts}"
    );
    assert!(ts.contains("export interface ActionResults {"), "{ts}");
    let count_result_key = format!("{}: number;", <count as Endpoint>::ACTION);
    assert!(
        ts.contains(&count_result_key),
        "expected `{count_result_key}` in TS: {ts}"
    ); // stream item type
    assert!(ts.contains("export interface ActionKinds {"), "{ts}");
    let count_kind_key = format!("{}: \"stream\";", <count as Endpoint>::ACTION);
    assert!(
        ts.contains(&count_kind_key),
        "expected `{count_kind_key}` in TS: {ts}"
    );
    assert!(
        ts.contains("export interface ClientActionParameters {"),
        "{ts}"
    );
    assert!(ts.contains("showToast: Toast;"), "{ts}");
    assert!(
        ts.contains("export interface ClientActionResults {"),
        "{ts}"
    );
    assert!(ts.contains("showToast: string;"), "{ts}");
    // the single inferable Router type
    assert!(ts.contains("export interface Router {"), "{ts}");
    assert!(ts.contains("serverActions:"), "{ts}");
    assert!(ts.contains("clientActions:"), "{ts}");
    // never malformed
    assert!(!ts.contains("= export interface"), "{ts}");
}
