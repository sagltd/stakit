//! End-to-end client tests against a real axum server. These mirror, case for
//! case, the shared matrix the TypeScript client is held to (see
//! `docs/transport.md` and `packages/client`).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::Json;
use axum::Router as AxumRouter;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;

use stakit_client::{ActionResult, CallOpts, Client, ServerFrame, TransportError};
use stakit_model::Model;
use stakit_router::{ClientAction, Cx, Error, Router, action};

// ── models ─────────────────────────────────────────────────────────────────

#[derive(Model, Serialize, Deserialize, Clone, Debug)]
struct Greet {
    #[validate(min_len = 1)]
    name: String,
}

#[derive(Model, Serialize, Deserialize, Clone, Debug)]
struct Greeting {
    message: String,
}

#[derive(Model, Serialize, Deserialize, Clone, Debug)]
struct SaveImage {
    file_name: String,
}

#[derive(Model, Serialize, Deserialize, Clone, Debug)]
struct Saved {
    bytes: u64,
}

#[derive(Model, Serialize, Deserialize, Clone, Debug)]
struct Count {
    n: u32,
}

#[derive(Model, Serialize, Deserialize, Clone, Debug)]
struct Toast {
    text: String,
}

// ── contexts + errors ────────────────────────────────────────────────────────

struct App {
    name: &'static str,
}

#[derive(Clone)]
struct Req {
    token: Option<String>,
    files: Vec<Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("boom: {0}")]
    Boom(String),
}

// ── server→client action ─────────────────────────────────────────────────────

struct ShowToast;
impl ClientAction for ShowToast {
    type Params = Toast;
    type Return = String;
    const NAME: &'static str = "showToast";
}

// ── actions ──────────────────────────────────────────────────────────────────

#[action]
async fn greet(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: format!("hello {} from {}", params.name, cx.app.name),
    })
}

#[action]
async fn whoami(cx: &Cx<App, Req>) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: cx.req.token.clone().unwrap_or_default(),
    })
}

#[action]
async fn boom() -> Result<Greeting, AppError> {
    Err(AppError::Boom("nope".to_owned()))
}

#[action]
async fn save_image(cx: &Cx<App, Req>, _params: SaveImage) -> Result<Saved, Error> {
    let bytes = cx.req.files.iter().map(Vec::len).sum::<usize>() as u64;
    Ok(Saved { bytes })
}

#[action(stream)]
fn count(_cx: &Cx<App, Req>, params: Count) -> impl Stream<Item = Result<u32, Error>> + use<> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

#[action(stream)]
fn failing(_cx: &Cx<App, Req>, _params: Count) -> impl Stream<Item = Result<u32, Error>> + use<> {
    async_stream::stream! {
        yield Ok(1u32);
        yield Err(Error::new(500, "mid-stream boom"));
    }
}

#[action(stream)]
fn progress<'a>(
    cx: &'a Cx<App, Req>,
    params: Count,
) -> impl Stream<Item = Result<u32, Error>> + 'a {
    async_stream::stream! {
        for i in 0..params.n {
            match cx.client_call::<ShowToast>(Toast { text: format!("step {i}") }).await {
                Ok(_ack) => yield Ok(i),
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
}

// ── axum harness ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Q {
    q: String,
}

fn build_router(name: &'static str) -> Router<App, Req> {
    Router::builder()
        .ctx(App { name })
        .register(greet)
        .register(whoami)
        .register(boom)
        .register(save_image)
        .register_stream(count)
        .register_stream(failing)
        .register_stream(progress)
        .build()
}

type Shared = State<Arc<Router<App, Req>>>;

async fn dispatch(
    router: &Router<App, Req>,
    headers: &HeaderMap,
    q: &str,
    files: Vec<Vec<u8>>,
) -> Json<Value> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let payload: Value = serde_json::from_str(q).unwrap_or(Value::Null);
    Json(router.on_request(Req { token, files }, payload).await)
}

async fn rpc(State(router): Shared, headers: HeaderMap, Query(q): Query<Q>) -> Json<Value> {
    dispatch(&router, &headers, &q.q, Vec::new()).await
}

async fn rpc_upload(
    State(router): Shared,
    headers: HeaderMap,
    Query(q): Query<Q>,
    mut multipart: Multipart,
) -> Json<Value> {
    let mut files = Vec::new();
    while let Some(field) = multipart.next_field().await.unwrap() {
        files.push(field.bytes().await.unwrap().to_vec());
    }
    dispatch(&router, &headers, &q.q, files).await
}

async fn stream_handler(State(router): Shared, Query(q): Query<Q>) -> Response {
    let payload: Value = serde_json::from_str(&q.q).unwrap_or(Value::Null);
    let frames = router
        .on_stream(
            Req {
                token: None,
                files: Vec::new(),
            },
            payload,
        )
        .map(|frame| {
            let mut bytes = serde_json::to_vec(&frame).unwrap();
            bytes.push(b'\n');
            Ok::<Bytes, std::io::Error>(Bytes::from(bytes))
        });
    Response::new(Body::from_stream(frames))
}

async fn ws_handler(State(router): Shared, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| handle_socket(router, socket))
}

async fn handle_socket(router: Arc<Router<App, Req>>, socket: WebSocket) {
    let mut session = router.session(Req {
        token: None,
        files: Vec::new(),
    });
    let mut outgoing = session.outgoing();
    let (mut sink, mut stream) = socket.split();

    let send_task = tokio::spawn(async move {
        while let Some(value) = outgoing.recv().await {
            let text = serde_json::to_string(&value).unwrap();
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = stream.next().await {
        if let Message::Text(text) = message {
            if let Ok(value) = serde_json::from_slice::<Value>(text.as_bytes()) {
                session.handle(&value);
            }
        }
    }
    send_task.abort();
}

/// Spawns a server and returns its origin (`http://127.0.0.1:PORT`).
async fn spawn_server(name: &'static str) -> String {
    let router = Arc::new(build_router(name));
    let app = AxumRouter::new()
        .route("/rpc", get(rpc).post(rpc_upload))
        .route("/stream", post(stream_handler))
        .route("/ws", get(ws_handler))
        .with_state(router);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn client_for(origin: &str) -> Client {
    Client::builder(format!("{origin}/rpc"))
        .header("authorization", "root")
        .stream_url(format!("{origin}/stream"))
        .ws_url(format!("{origin}/ws"))
        .build()
}

// ── tests (shared matrix) ─────────────────────────────────────────────────────

#[tokio::test] // 1
async fn unary_ok_returns_typed_data() {
    let client = client_for(&spawn_server("A").await);
    let result = client
        .fetch(
            greet,
            Greet {
                name: "sam".to_owned(),
            },
        )
        .await
        .unwrap();
    assert!(result.is_ok());
    assert_eq!(result.data().unwrap().message, "hello sam from A");
}

#[tokio::test] // 2
async fn unary_app_error_is_action_error() {
    let client = client_for(&spawn_server("A").await);
    let result = client.fetch(boom, ()).await.unwrap();
    assert!(result.is_error());
    let error = result.error().unwrap();
    assert_eq!(error.code, 500);
    assert!(error.message.contains("boom"));
}

#[tokio::test] // 3
async fn validation_error_has_fields() {
    let client = client_for(&spawn_server("A").await);
    let result = client
        .fetch(
            greet,
            Greet {
                name: String::new(),
            },
        )
        .await
        .unwrap();
    let error = result.error().unwrap();
    assert_eq!(error.code, 422);
    assert!(error.fields.as_ref().unwrap().contains_key("name"));
}

#[tokio::test] // 5
async fn stream_yields_items_then_ends() {
    let client = client_for(&spawn_server("A").await);
    let stream = client.stream(count, Count { n: 4 }).await.unwrap();
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 4);
    let values: Vec<u32> = items.into_iter().filter_map(ActionResult::ok).collect();
    assert_eq!(values, vec![0, 1, 2, 3]);
}

#[tokio::test] // 6
async fn stream_error_terminates() {
    let client = client_for(&spawn_server("A").await);
    let stream = client.stream(failing, Count { n: 9 }).await.unwrap();
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_ok());
    assert!(items[1].is_error());
}

#[tokio::test] // 7
async fn websocket_roundtrip() {
    let client = client_for(&spawn_server("A").await);
    let mut conn = client.connect(CallOpts::new()).await.unwrap();
    conn.send(
        greet,
        Greet {
            name: "ws".to_owned(),
        },
    )
    .await
    .unwrap();
    let frame = conn.recv().await.unwrap().unwrap();
    let ServerFrame::Result { result, .. } = frame else {
        panic!("expected a result frame, got {frame:?}");
    };
    let greeting: Greeting = serde_json::from_value(result.ok().unwrap()).unwrap();
    assert_eq!(greeting.message, "hello ws from A");
    conn.close().await.unwrap();
}

#[tokio::test] // 8
async fn websocket_server_to_client_call() {
    let client = client_for(&spawn_server("A").await);
    let mut conn = client.connect(CallOpts::new()).await.unwrap();
    conn.send(progress, Count { n: 2 }).await.unwrap();

    let mut toasts = 0;
    let mut results = 0;
    loop {
        match conn.recv().await {
            Some(Ok(ServerFrame::ClientCall { id, action, .. })) => {
                assert_eq!(action, "showToast");
                toasts += 1;
                conn.reply(id, "ok").await.unwrap();
            }
            Some(Ok(ServerFrame::Result { result, .. })) => {
                assert!(result.is_ok());
                results += 1;
            }
            Some(Ok(ServerFrame::End { .. })) | None => break,
            Some(Err(error)) => panic!("ws error: {error}"),
        }
    }
    assert_eq!(toasts, 2);
    assert_eq!(results, 2);
}

#[tokio::test] // 9
async fn per_call_url_override_leaves_base_untouched() {
    let origin_b = spawn_server("B").await;
    let client = client_for(&spawn_server("A").await);

    let base = client
        .fetch(
            greet,
            Greet {
                name: "x".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(base.data().unwrap().message, "hello x from A");

    let overridden = client
        .fetch_with(
            greet,
            Greet {
                name: "x".to_owned(),
            },
            CallOpts::new().url(format!("{origin_b}/rpc")),
        )
        .await
        .unwrap();
    assert_eq!(overridden.data().unwrap().message, "hello x from B");

    // base url unchanged
    let again = client
        .fetch(
            greet,
            Greet {
                name: "x".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(again.data().unwrap().message, "hello x from A");
}

#[tokio::test] // 10
async fn per_call_headers_merge_and_leave_base_untouched() {
    let client = client_for(&spawn_server("A").await);

    let base = client.fetch(whoami, ()).await.unwrap();
    assert_eq!(base.data().unwrap().message, "root");

    let overridden = client
        .fetch_with(
            whoami,
            (),
            CallOpts::new().header("authorization", "scoped"),
        )
        .await
        .unwrap();
    assert_eq!(overridden.data().unwrap().message, "scoped");

    let again = client.fetch(whoami, ()).await.unwrap();
    assert_eq!(again.data().unwrap().message, "root");
}

#[tokio::test] // 11
async fn files_upload_via_multipart() {
    let client = client_for(&spawn_server("A").await);
    let result = client
        .fetch_with(
            save_image,
            SaveImage {
                file_name: "a.png".to_owned(),
            },
            CallOpts::new()
                .file(vec![1u8, 2, 3, 4, 5])
                .file(vec![6u8, 7, 8]),
        )
        .await
        .unwrap();
    assert_eq!(result.data().unwrap().bytes, 8);
}

#[tokio::test] // 12
async fn set_headers_replace_and_update() {
    let client = client_for(&spawn_server("A").await);

    client.set_headers(vec![("authorization".to_owned(), "replaced".to_owned())]);
    let replaced = client.fetch(whoami, ()).await.unwrap();
    assert_eq!(replaced.data().unwrap().message, "replaced");

    client.update_headers(|headers| {
        for (name, value) in headers.iter_mut() {
            if name == "authorization" {
                *value = "updated".to_owned();
            }
        }
    });
    let updated = client.fetch(whoami, ()).await.unwrap();
    assert_eq!(updated.data().unwrap().message, "updated");
}

#[tokio::test] // 14: many actions in one request (typed batch)
async fn batch_runs_multiple_actions_in_one_request() {
    let client = client_for(&spawn_server("A").await);
    let results = client
        .batch()
        .add(
            greet,
            Greet {
                name: "a".to_owned(),
            },
        )
        .add(
            greet,
            Greet {
                name: "b".to_owned(),
            },
        )
        .add(whoami, ())
        .send()
        .await
        .unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(
        results.get::<Greeting>(0).unwrap().data().unwrap().message,
        "hello a from A"
    );
    assert_eq!(
        results.get::<Greeting>(1).unwrap().data().unwrap().message,
        "hello b from A"
    );
    // whoami echoes the base `authorization` header ("root").
    assert_eq!(
        results.get::<Greeting>(2).unwrap().data().unwrap().message,
        "root"
    );
}

#[tokio::test] // 15: real wss:// TLS handshake against a public echo server
#[ignore = "network: connects to a public wss echo server"]
async fn wss_tls_handshake_works() {
    let client = Client::new("https://unused.invalid");
    let conn = client
        .connect(CallOpts::new().url("wss://echo.websocket.events"))
        .await;
    assert!(conn.is_ok(), "wss handshake failed: {:?}", conn.err());
    conn.unwrap().close().await.ok();
}

#[tokio::test] // 13
async fn transport_failure_is_err() {
    // Port 1 refuses connections.
    let client = Client::new("http://127.0.0.1:1/rpc");
    let result = client
        .fetch(
            greet,
            Greet {
                name: "x".to_owned(),
            },
        )
        .await;
    assert!(matches!(result, Err(TransportError::Http(_))));
}
