//! Full axum integration for `stakit-router`, **payload-routed**: the action
//! name lives inside the request payload, so the whole app is served through a
//! handful of endpoints (one per transport), each delegating the entire payload
//! to the router:
//!
//! - `GET  /app`    — unary calls, payload in `?q=<json>`
//! - `POST /app`    — unary calls + multipart file upload (`?q=` + `file` parts)
//! - `POST /stream` — streaming calls (JSONL frames)
//! - `GET  /ws`     — duplex websocket (incl. server→client `client_call`)
//!
//! A real HTTP-only app can collapse to a **single** `/app` route. The payload is
//! either an object `{ "greet": {…} }` or an ordered array `[["greet", {…}]]`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router as AxumRouter;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::Value;
use stakit_model::model;
use stakit_router::{ClientAction, Cx, Error, Router, action};

// ---- global context ----
/// Shared application state.
pub struct App {
    /// Greeting prefix.
    pub greeting: String,
}

// ---- per-request context: holds anything, incl. uploaded image bytes ----
/// Per-request context.
#[derive(Clone, Default)]
pub struct Req {
    /// Whether the caller sent `x-admin: true`.
    pub admin: bool,
    /// Concatenated bytes of any uploaded files.
    pub image: Vec<u8>,
}

// ---- models (one `#[model]` = Model + serde + camelCase, no footgun) ----
/// Greeting parameters.
#[model]
pub struct Greet {
    /// Who to greet.
    #[validate(min_len = 1, max_len = 20)]
    pub name: String,
    /// Optional user id.
    pub user_id: Option<u64>,
}

/// A greeting.
#[model]
pub struct Greeting {
    /// The greeting message.
    pub message: String,
}

/// How many items to stream.
#[model]
pub struct Count {
    /// Item count.
    pub n: u64,
}

/// Image-save parameters.
#[model]
pub struct SaveImage {
    /// Destination file name.
    #[validate(min_len = 1)]
    pub file_name: String,
}

/// Result of saving an image.
#[model]
pub struct Saved {
    /// Bytes written.
    pub bytes: u64,
    /// Where it was written.
    pub path: String,
}

// ---- client action (server -> client, used over websocket) ----
/// A toast to show on the client.
#[model]
pub struct Toast {
    /// Toast text.
    pub text: String,
}

/// The `showToast` client action.
pub struct ShowToast;
impl ClientAction for ShowToast {
    type Params = Toast;
    type Return = String;
    const NAME: &'static str = "showToast";
}

// ---- actions ----
/// Greets someone.
#[action]
pub async fn greet(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: format!("{}, {}! (admin={})", cx.app.greeting, params.name, cx.req.admin),
    })
}

/// Returns the server version (param-less, ctx-less unary action).
#[action]
pub async fn version() -> Result<String, Error> {
    Ok("stakit-example/0.1.0".to_owned())
}

/// Streams `0..n`.
#[action(stream)]
pub fn count(params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

/// Unary action that calls back to the client (duplex only).
#[action]
pub async fn notify(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    let ack = cx.client_call::<ShowToast>(Toast { text: params.name }).await?;
    Ok(Greeting { message: ack })
}

/// Streams `0..n`, asking the client to show a toast before each item.
#[action(stream)]
pub fn progress(cx: &Cx<App, Req>, params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            match cx.client_call::<ShowToast>(Toast { text: format!("step {i}") }).await {
                Ok(_ack) => yield Ok(i),
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
    }
}

/// Saves uploaded image bytes (params from `?q=`, bytes from the multipart body).
#[action]
pub async fn save_image(cx: &Cx<App, Req>, params: SaveImage) -> Result<Saved, Error> {
    if cx.req.image.is_empty() {
        return Err(Error::new(400, "no image in request body"));
    }
    let path = format!("/tmp/stakit-upload-{}", params.file_name);
    std::fs::write(&path, &cx.req.image)?; // io::Error -> Error (500) via `?`
    Ok(Saved {
        bytes: cx.req.image.len() as u64,
        path,
    })
}

/// Shared axum state holding the router.
#[derive(Clone)]
pub struct AppState {
    /// The router.
    pub router: Arc<Router<App, Req>>,
}

/// Builds the example router.
#[must_use]
pub fn build_router() -> Router<App, Req> {
    Router::builder()
        .ctx(App {
            greeting: "Hello".to_owned(),
        })
        .register(greet)
        .register(version)
        .register(notify)
        .register(save_image)
        .register_stream(count)
        .register_stream(progress)
        .client_action::<ShowToast>()
        .build()
}

/// Builds the axum app (the whole API behind a few payload-routed endpoints).
pub fn app() -> AxumRouter {
    let state = AppState {
        router: Arc::new(build_router()),
    };
    AxumRouter::new()
        .route("/app", get(app_get).post(app_post))
        .route("/stream", post(stream_handler))
        .route("/ws", get(ws))
        .with_state(state)
}

#[derive(Deserialize)]
struct Q {
    q: Option<String>,
}

fn req_from(headers: &HeaderMap, image: Vec<u8>) -> Req {
    Req {
        admin: headers.get("x-admin").is_some_and(|v| v == "true"),
        image,
    }
}

fn payload_from(q: &Q) -> Value {
    q.q.as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(Value::Null)
}

// ---- unary: GET /app (no body) ----
async fn app_get(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<Q>) -> Response {
    let response = state
        .router
        .on_request(req_from(&headers, Vec::new()), payload_from(&q))
        .await;
    axum::Json(response).into_response()
}

// ---- unary + files: POST /app?q={json} (multipart `file` parts) ----
async fn app_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Q>,
    mut multipart: Multipart,
) -> Response {
    let mut image = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        if let Ok(bytes) = field.bytes().await {
            image.extend_from_slice(&bytes);
        }
    }
    let response = state
        .router
        .on_request(req_from(&headers, image), payload_from(&q))
        .await;
    axum::Json(response).into_response()
}

// ---- stream: POST /stream (JSONL frames) ----
async fn stream_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> Response {
    let frames = state
        .router
        .on_stream(req_from(&headers, Vec::new()), payload_from(&q))
        .map(|frame| {
            let mut bytes = serde_json::to_vec(&frame).unwrap_or_default();
            bytes.push(b'\n');
            Ok::<Bytes, Infallible>(Bytes::from(bytes))
        });
    Response::new(Body::from_stream(frames))
}

// ---- websocket duplex: GET /ws ----
async fn ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let req = req_from(&headers, Vec::new());
    upgrade.on_upgrade(move |socket| handle_ws(state, req, socket))
}

async fn handle_ws(state: AppState, req: Req, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let mut session = state.router.session(req);
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    let send = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if sink
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Text(text) = msg
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            session.handle(&value);
        }
    }
    send.abort();
}
