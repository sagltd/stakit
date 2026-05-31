//! Full axum integration for `stakit-router`: HTTP unary, SSE streaming, and a
//! WebSocket duplex endpoint (with server→client `client_call`).
//!
//! Run: `cargo run` (from this dir). Then:
//!   curl -s localhost:3007/rpc/greet -H 'x-admin: true' -d '{"name":"bob"}'
//!   curl -s localhost:3007/sse/count -d '{"n":3}'
//!   websocket: connect ws://localhost:3007/ws and send
//!     {"kind":"call","id":1,"action":"greet","params":{"name":"ada"}}

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router as AxumRouter;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stakit_model::Model;
use stakit_router::{Cx, Error, Router, action};

// ---- contexts ----
struct App {
    greeting: String,
}

#[derive(Clone)]
struct Auth {
    admin: bool,
}

// ---- models ----
#[derive(Model, Serialize, Deserialize)]
struct Greet {
    #[validate(min_len = 1, max_len = 20)]
    name: String,
    user_id: Option<u64>,
}

#[derive(Model, Serialize)]
struct Greeting {
    message: String,
}

#[derive(Model, Serialize, Deserialize)]
struct Count {
    n: u64,
}

// ---- actions ----
#[action]
async fn greet(cx: &Cx<App, Auth>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting {
        message: format!(
            "{}, {}! (admin={})",
            cx.app.greeting, params.name, cx.req.admin
        ),
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

#[derive(Clone)]
struct AppState {
    router: Arc<Router<App, Auth>>,
}

fn auth_from(headers: &HeaderMap) -> Auth {
    Auth {
        admin: headers.get("x-admin").is_some_and(|v| v == "true"),
    }
}

// ---- HTTP unary: POST /rpc/{action} ----
async fn rpc(
    State(state): State<AppState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    axum::Json(params): axum::Json<Value>,
) -> Response {
    let reply = state
        .router
        .on_request(auth_from(&headers), &action, params)
        .await;
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
        .on_stream(auth_from(&headers), &action, params)
        .map(|frame| Ok(Event::default().json_data(frame).unwrap_or_default()));
    Sse::new(stream)
}

// ---- WebSocket duplex: GET /ws ----
async fn ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let auth = auth_from(&headers);
    upgrade.on_upgrade(move |socket| handle_ws(state, auth, socket))
}

async fn handle_ws(state: AppState, auth: Auth, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let mut session = state.router.session(auth);
    let mut outgoing = session.outgoing();
    let session = Arc::new(session);

    // server → socket
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

    // socket → session
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
        .ctx(App {
            greeting: "Hello".to_owned(),
        })
        .register(greet)
        .register_stream(count)
        .build();

    // Generate the TypeScript client types next to this example.
    std::fs::write("types.d.ts", router.generate_ts()).expect("write types.d.ts");
    println!("wrote types.d.ts");

    let state = AppState {
        router: Arc::new(router),
    };
    let app = AxumRouter::new()
        .route("/rpc/{action}", post(rpc))
        .route("/sse/{action}", post(sse))
        .route("/ws", get(ws))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3007")
        .await
        .unwrap();
    println!("listening on http://127.0.0.1:3007");
    axum::serve(listener, app).await.unwrap();
}
