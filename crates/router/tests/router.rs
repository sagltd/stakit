//! End-to-end tests for `#[action]` + `Router`, covering every signature shape,
//! validation, action-to-action calls, and TypeScript generation.
#![allow(dead_code)]

use std::sync::Arc;

use futures::StreamExt as _;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::json;
use stakit_model::Model;
use stakit_router::{ClientAction, Cx, Error, Frame, Reply, Router, action, err};

// --- contexts ---
struct App {
    greeting: String,
}
#[derive(Clone)]
struct Auth {
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
async fn greet(cx: &Cx<App, Auth>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: format!("{}, {}!", cx.app.greeting, params.name),
    })
}

#[action]
fn ping() -> Result<String, Error> {
    Ok("pong".to_owned())
}

#[action]
async fn whoami(cx: &Cx<App, Auth>) -> Result<bool, Error> {
    Ok(cx.req.admin)
}

#[action]
async fn greet_twice(cx: &Cx<App, Auth>, params: Greet) -> Result<String, Error> {
    // action-to-action call, typed, still validated
    let g = cx.call(greet, params).await?;
    Ok(format!("{} {}", g.message, g.message))
}

// --- custom application error: returned straight from an action ---
#[derive(Debug)]
struct AppError(&'static str);
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for AppError {}

#[action]
async fn maybe_fail(params: Greet) -> Result<Greeting, AppError> {
    if params.name == "boom" {
        return Err(AppError("kaboom")); // any std error -> 500
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

// --- thiserror-defined app error works out of the box ---
#[derive(Debug, thiserror::Error)]
enum TodoError {
    #[error("database is down")]
    Db,
}

#[action]
async fn risky(_params: Greet) -> Result<Greeting, TodoError> {
    Err(TodoError::Db) // thiserror -> std::error::Error -> Into<Error> (500)
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
async fn notify_user(cx: &Cx<App, Auth>, params: Greet) -> Result<Greeting, Error> {
    let ack: String = cx
        .client_call::<ShowToast>(Toast { text: params.name })
        .await?;
    Ok(Greeting { message: ack })
}

fn router() -> Router<App, Auth> {
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
        .client_action::<ShowToast>()
        .build()
}

#[test]
fn dispatches_and_runs_with_ctx() {
    let reply =
        block_on(router().on_request(Auth { admin: true }, "greet", json!({"name": "bob"})));
    match reply {
        Reply::Ok { data } => assert_eq!(data, json!({"message": "Hello, bob!"})),
        Reply::Error { .. } => panic!("expected ok: {reply:?}"),
    }
}

#[test]
fn param_less_and_ctx_less_action() {
    let reply = block_on(router().on_request(Auth { admin: false }, "ping", json!(null)));
    assert!(matches!(reply, Reply::Ok { data } if data == json!("pong")));
}

#[test]
fn ctx_only_action_reads_request_ctx() {
    let reply = block_on(router().on_request(Auth { admin: true }, "whoami", json!(null)));
    assert!(matches!(reply, Reply::Ok { data } if data == json!(true)));
}

#[test]
fn invalid_params_yield_validation_error() {
    let reply = block_on(router().on_request(Auth { admin: true }, "greet", json!({"name": ""})));
    match reply {
        Reply::Error { error } => {
            assert_eq!(error.code, 422);
            assert!(error.fields.unwrap().contains_key("name"));
        }
        Reply::Ok { .. } => panic!("expected validation error"),
    }
}

#[test]
fn unknown_action_is_404() {
    let reply = block_on(router().on_request(Auth { admin: true }, "nope", json!(null)));
    assert!(matches!(reply, Reply::Error { error } if error.code == 404));
}

#[test]
fn action_to_action_call() {
    let reply =
        block_on(router().on_request(Auth { admin: true }, "greet_twice", json!({"name": "sam"})));
    match reply {
        Reply::Ok { data } => assert_eq!(data, json!("Hello, sam! Hello, sam!")),
        Reply::Error { .. } => panic!("expected ok"),
    }
}

// Mirrors how you'd wire this into axum: the router lives in shared state; an
// HTTP handler extracts the request ctx + already-decoded body, calls
// `on_request`, and maps the `Reply` to (status, json). Real axum is the same
// few lines — without the dependency here.
#[derive(Clone)]
struct AppState {
    router: Arc<Router<App, Auth>>,
}

fn http_handler(
    state: &AppState,
    headers: &[(&str, &str)],
    action: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let admin = headers.iter().any(|(k, v)| *k == "x-admin" && *v == "true");
    let reply = block_on(state.router.on_request(Auth { admin }, action, body));
    let code = reply.code();
    (code, serde_json::to_value(reply).unwrap())
}

#[test]
fn axum_style_wiring() {
    let state = AppState {
        router: Arc::new(router()),
    };

    let (code, body) = http_handler(
        &state,
        &[("x-admin", "true")],
        "greet",
        json!({"name": "sam"}),
    );
    assert_eq!(code, 200);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["data"]["message"], "Hello, sam!");

    let (code, body) = http_handler(&state, &[], "greet", json!({"name": ""}));
    assert_eq!(code, 422);
    assert_eq!(body["status"], "error");
}

#[test]
fn custom_app_error_propagates_as_500() {
    let reply =
        block_on(router().on_request(Auth { admin: true }, "maybe_fail", json!({"name": "boom"})));
    match reply {
        Reply::Error { error } => {
            assert_eq!(error.code, 500);
            assert_eq!(error.message, "kaboom");
        }
        Reply::Ok { .. } => panic!("expected error"),
    }
}

#[test]
fn explicit_error_code_via_macro() {
    let reply =
        block_on(router().on_request(Auth { admin: true }, "find", json!({"name": "missing"})));
    assert!(matches!(reply, Reply::Error { error } if error.code == 404));
}

#[test]
fn streaming_action_yields_frames() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream(Auth { admin: true }, "count", json!({"n": 3}))
            .collect(),
    );
    assert_eq!(frames.len(), 4); // 3 items + End
    match &frames[0] {
        Frame::Next { data } => assert_eq!(data, &json!(0)),
        _ => panic!("expected Next"),
    }
    assert!(matches!(frames[3], Frame::End));
}

#[test]
fn streaming_unknown_action_errors() {
    let frames: Vec<Frame> = block_on(
        router()
            .on_stream(Auth { admin: true }, "nope", json!(null))
            .collect(),
    );
    assert!(matches!(frames.first(), Some(Frame::Error { error }) if error.code == 404));
}

#[test]
fn thiserror_app_error_works_out_of_the_box() {
    let reply = block_on(router().on_request(Auth { admin: true }, "risky", json!({"name": "x"})));
    match reply {
        Reply::Error { error } => {
            assert_eq!(error.code, 500);
            assert_eq!(error.message, "database is down");
        }
        Reply::Ok { .. } => panic!("expected error"),
    }
}

#[tokio::test]
async fn duplex_client_call_roundtrip() {
    let router = Arc::new(router());
    let mut session = router.session(Auth { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    // Client invokes the server action `notify_user`.
    session.handle(
        &json!({ "kind": "call", "id": 1, "action": "notify_user", "params": { "name": "bob" } }),
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
    let mut session = router.session(Auth { admin: true });
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    session.handle(&json!({ "kind": "call", "id": 7, "action": "count", "params": { "n": 2 } }));

    let f0 = outgoing.recv().await.unwrap();
    assert_eq!(f0["kind"], "result");
    assert_eq!(f0["data"], json!(0));
    let _f1 = outgoing.recv().await.unwrap();
    let end = outgoing.recv().await.unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["id"], 7);
}

#[test]
fn generates_typescript() {
    let ts = router().generate_ts();
    // model declarations
    assert!(ts.contains("export interface Greet {"), "{ts}");
    assert!(ts.contains("export interface Greeting {"), "{ts}");
    assert!(ts.contains("export interface Toast {"), "{ts}");
    assert!(ts.contains("message: string"), "{ts}");
    // typed maps
    assert!(ts.contains("export interface ActionParameters {"), "{ts}");
    assert!(ts.contains("greet: Greet;"), "{ts}");
    assert!(ts.contains("export interface ActionResults {"), "{ts}");
    assert!(ts.contains("count: number;"), "{ts}"); // stream item type
    assert!(ts.contains("export interface ActionKinds {"), "{ts}");
    assert!(ts.contains("count: \"stream\";"), "{ts}");
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
