# stakit-router

Framework- and format-agnostic **action router** for Rust: validated input
(via `stakit-model`), payload-based routing, typed action-to-action calls,
duplex websocket sessions with server→client calls, and TypeScript client
generation. It knows nothing about HTTP/WebSockets specifically — you hand it
already-decoded params + a request context and wire the result into any
framework (axum, hyper, …).

A single `stakit-router` dependency re-exports the model traits/macros too, so
one import gives you the whole surface.

## Define actions

`#[action]` turns a free function into a named action. The signature is flexible
— async or sync, with any of `(cx, params)`, `(params)`, `(cx)`, or `()`:

```rust
use stakit_router::{action, model, Cx, Error};

// `#[model]` = Model (validation) + serde + camelCase TS, in one.
#[model]
struct Greet {
    #[validate(min_len = 1, max_len = 20)]
    name: String,
}

#[model]
struct Greeting {
    message: String,
}

struct App { greeting: String }   // G — app/global state: db pool, config, … (made once, shared)
struct Req { admin: bool }        // R — per-request context: current user, headers, … (built per call)

#[action]
async fn greet(cx: &Cx<App, Req>, params: Greet) -> Result<Greeting, Error> {
    Ok(Greeting { message: format!("{}, {}!", cx.app.greeting, params.name) })
}

// streaming action — yields a stream of items
#[action(stream)]
fn count(params: Greet) -> impl futures::Stream<Item = Result<u64, Error>> {
    async_stream::stream! { for i in 0..3 { yield Ok(i); } }
}
```

The error type is taken from the return, so actions return **their own** error
(anything `Into<Error>`; any `std::error::Error` / `thiserror` works, defaulting
to 500). Use `err!(code, msg)` for an explicit code.

## The context: `Cx<G, R>`

Every action gets a `&Cx<G, R>`. Two generics, both **your** types:

- `cx.app: Arc<G>` — **app / global state**: made once at `Router::build`, shared
  across every request. This is where your **database connection pool**, config,
  HTTP clients and caches live.
- `cx.req: R` — **per-request context**: built fresh for each request — the
  current user/auth, request headers, uploaded files, a request id, …
- `cx.call(other_action, input)` — typed action→action call, in-process.
- `cx.client_call::<C>(params)` — server→client call (duplex only).

## Build a router, serve it through one endpoint

The action name lives **in the payload**, never the URL — so the whole API is
served by a few endpoints (one per transport). A payload is an object
`{ "greet": {…} }` or an ordered array `[["greet", {…}], …]`.

```rust
let router = Router::builder()
    .ctx(App { greeting: "Hello".into() })
    .register(greet)
    .register_stream(count)
    .client_call_timeout(std::time::Duration::from_secs(15)) // default 30s
    .build();

// unary: feed the decoded payload, get a JSON response value back
let response = router.on_request(req_ctx, payload).await;     // axum: Json(response)

// stream: a `'static` stream of frames → Body::from_stream / SSE
let frames = router.on_stream(req_ctx, payload);

// duplex (websocket): a session you pump frames in/out of
let session = router.session(req_ctx);
```

See `examples/axum-server` for the full wiring (one `/app` route + `/stream` +
`/ws`) and the matching Rust + TypeScript clients.

## Testing actions — no server needed

This is the point: an action is a plain function over a context. Build a `Cx`
with **`Cx::test(app, req)`** and call the action directly — no router, no HTTP.

```rust
use stakit_router::{Action, Cx};   // `Action` brings `.run()` into scope

#[tokio::test]
async fn greet_works() {
    let cx = Cx::test(App { greeting: "Hi".into() }, Req { admin: true });

    let out = greet.run(&cx, Greet { name: "bob".into() }).await.unwrap();

    assert_eq!(out.message, "Hi, bob!");
}
```

`greet` is the unit struct `#[action]` generates; `.run(&cx, input)` runs the
body and returns `Result<Output, YourError>`.

### Action → action

```rust
let cx = Cx::test(App { greeting: "Hi".into() }, Req { admin: true });
let out = cx.call(greet, Greet { name: "sam".into() }).await.unwrap();
```

### Actions that use `client_call` — stub the client

Chain **`.with_client(handler)`** onto `Cx::test`. The handler gets the action
name + JSON params and returns the JSON reply (use `serde_json::json!` /
`to_value` for typed returns):

```rust
use serde_json::json;

#[tokio::test]
async fn notify_works() {
    let cx = Cx::test(App { greeting: "Hi".into() }, Req { admin: true })
        .with_client(|name, _params| {
            assert_eq!(name, "showToast");
            Ok(json!("delivered"))
        });

    let out = notify.run(&cx, Greet { name: "x".into() }).await.unwrap();
    assert_eq!(out.message, "delivered");
}
```

### What `Cx::test` does and doesn't cover

- `.run(&cx, input)` tests your **logic** with already-typed input.
- Input **validation** runs at the transport boundary, not in `run`. Test it
  directly with `params.validate()` (from `Model`), or go through
  `router.on_request(...)` for a full-path test (deserialize → validate → run).
- `client_call` errors on a plain `Cx::test` (no client); use `.with_client(..)`.

### End-to-end (server + client)

For a full-stack test, spin the real app and drive it with `stakit-client` — see
`examples/axum-server/tests/e2e.rs` (http unary, multi-action batch, multipart
files, stream, and websocket + `client_call`, all over the wire).

## Transports & `client_call`

| transport | entry | server→client `client_call`? |
|-----------|-------|------------------------------|
| HTTP unary | `on_request(req, payload)` | no (one-way) — errors `400` |
| HTTP stream | `on_stream(req, payload)` | no (one-way) — errors `400` |
| WebSocket | `session(req)` | **yes** (even inside a streaming action) |

`client_call` waits for the client's reply up to the configured timeout
(default [`DEFAULT_CLIENT_CALL_TIMEOUT`] = 30s, override with
`Builder::client_call_timeout`), then fails `504` and drops the pending entry —
a silent client can't leak the suspended task.

## TypeScript client

`router.generate_ts()` emits a `Router` type (`ActionParameters`,
`ActionResults`, `ActionKinds`, `ClientAction*`) that the `@stakit/client`
TypeScript package is generic over. See `docs/transport.md` for the wire
contract shared by every client.
