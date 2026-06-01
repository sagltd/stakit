//! Full axum integration for `stakit-router`: HTTP unary, SSE streaming, a
//! WebSocket duplex endpoint (with server→client `client_call`), and a binary
//! **image upload** (JSON params from the `?q=` query, raw image from the body).
//!
//! Run: `cargo run`. Then:
//!   curl localhost:3007/rpc/greet -H 'content-type: application/json' -d '{"name":"bob"}'
//!   curl localhost:3007/sse/count  -H 'content-type: application/json' -d '{"n":3}'
//!   curl 'localhost:3007/upload/save_image?q={"fileName":"pic.png"}' --data-binary @pic.png

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::Router as AxumRouter;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use stakit_model::model;
use stakit_router::{ClientAction, Cx, Error, Router, action};

// ---- global context ----
struct App {
    greeting: String,
}

// ---- per-request context: holds anything, incl. the raw uploaded image ----
#[derive(Clone, Default)]
struct Req {
    admin: bool,
    image: Vec<u8>,
}

// ---- models (one `#[model]` = Model + serde + camelCase, no footgun) ----
#[model]
struct Greet {
    #[validate(min_len = 1, max_len = 20)]
    name: String,
    user_id: Option<u64>,
}

#[model]
struct Greeting {
    message: String,
}

#[model]
struct Count {
    n: u64,
}

#[model]
struct SaveImage {
    #[validate(min_len = 1)]
    file_name: String,
}

#[model]
struct Saved {
    bytes: u64,
    path: String,
}

// ---- client action (server -> client, used over websocket) ----
#[model]
struct Toast {
    text: String,
}

struct ShowToast;
impl ClientAction for ShowToast {
    type Params = Toast;
    type Return = String;
    const NAME: &'static str = "showToast";
}

// ---- actions ----
#[action]
async fn greet(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: format!("{}, {}! (admin={})", cx.app.greeting, params.name, cx.req.admin),
    })
}

#[action(stream)]
fn count(params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! {
        for i in 0..params.n {
            yield Ok(i);
        }
    }
}

#[action]
async fn notify(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    let ack = cx.client_call::<ShowToast>(Toast { text: params.name }).await?;
    Ok(Greeting { message: ack })
}

/// Streaming action that also calls a client action on every iteration: it
/// streams `0..n`, and before each item asks the client (over duplex) to show a
/// toast and awaits the reply. Streaming + `client_call` in a loop.
#[action(stream)]
fn progress(cx: &Cx<App, Req>, params: Count) -> impl futures::Stream<Item = Result<u64, Error>> {
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

/// Params (filename) come from the query `?q=`; the image bytes come from the
/// request body via `cx.req.image`.
#[action]
async fn save_image(cx: &Cx<App, Req>, params: SaveImage) -> Result<Saved, Error> {
    if cx.req.image.is_empty() {
        return Err(Error::new(400, "no image in request body"));
    }
    let path = format!("/tmp/stakit-upload-{}", params.file_name);
    std::fs::write(&path, &cx.req.image)?; // io::Error -> Error (500) via `?`
    Ok(Saved { bytes: cx.req.image.len() as u64, path })
}

#[derive(Clone)]
struct AppState {
    router: Arc<Router<App, Req>>,
}

fn req_from(headers: &HeaderMap, image: Vec<u8>) -> Req {
    Req { admin: headers.get("x-admin").is_some_and(|v| v == "true"), image }
}

// ---- HTTP unary: POST /rpc/{action} (JSON body = params) ----
async fn rpc(
    State(state): State<AppState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    axum::Json(params): axum::Json<Value>,
) -> Response {
    let reply = state.router.on_request(req_from(&headers, Vec::new()), &action, params).await;
    let status = StatusCode::from_u16(reply.code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(reply)).into_response()
}

// ---- image upload: POST /upload/{action}?q={json} (body = raw image) ----
async fn upload(
    State(state): State<AppState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let params: Value =
        query.get("q").and_then(|q| serde_json::from_str(q).ok()).unwrap_or(Value::Null);
    let reply =
        state.router.on_request(req_from(&headers, body.to_vec()), &action, params).await;
    let status = StatusCode::from_u16(reply.code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(reply)).into_response()
}

// ---- SSE stream: POST /sse/{action} ----
async fn sse(
    State(state): State<AppState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    axum::Json(params): axum::Json<Value>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let stream = state
        .router
        .on_stream(req_from(&headers, Vec::new()), &action, params)
        .map(|frame| Ok(Event::default().json_data(frame).unwrap_or_default()));
    Sse::new(stream)
}

// ---- WebSocket duplex: GET /ws ----
async fn ws(State(state): State<AppState>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
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
            if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
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

#[tokio::main]
async fn main() {
    let router = Router::builder()
        .ctx(App { greeting: "Hello".to_owned() })
        .register(greet)
        .register(notify)
        .register(save_image)
        .register_stream(count)
        .register_stream(progress)
        .client_action::<ShowToast>()
        .build();

    std::fs::write("types.d.ts", router.generate_ts()).expect("write types.d.ts");
    println!("wrote types.d.ts");

    let state = AppState { router: Arc::new(router) };
    let app = AxumRouter::new()
        .route("/rpc/{action}", post(rpc))
        .route("/upload/{action}", post(upload))
        .route("/sse/{action}", post(sse))
        .route("/ws", get(ws))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3007").await.unwrap();
    println!("listening on http://127.0.0.1:3007");
    axum::serve(listener, app).await.unwrap();
}
