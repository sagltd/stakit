# stakit-router — Architecture

Framework- and format-agnostic action router. Does **validation + routing +
action↔action calls + client-actions + TypeScript generation**. Knows nothing
about sockets or JSON — you wire it into axum/hyper/ws/etc. Inspired by ggtype's
`createRouter` (`onRequest` / `onWebSocketMessage`), but typed and decoupled.

## Authoring: `#[action]`

Named async/sync fns become actions. The macro adapts to the signature shape:

```rust
#[action] async fn get_user(cx: &Cx<App, Auth>, params: GetUser) -> Result<User> { … }
#[action] fn ping() -> Result<&'static str> { Ok("pong") }          // sync, no cx, no params
#[action] async fn list(params: Page) -> Result<Vec<Item>> { … }    // no cx
#[action(stream)] async fn chat(cx: &Cx<App, Auth>, params: Prompt)
    -> Result<impl Stream<Item = Result<Token>>> { … }
```

The macro emits a ZST + `impl Action<G, R>`:
- `type Input` = the `params` type (or `()` if absent); `Input: Model + DeserializeOwned`.
- `type Output`; `Output: TSType + Serialize`.
- `const NAME` = the fn name; `KIND` = Unary | Stream.
- `async fn run(&self, cx, input) -> Response<Output>` — sync bodies are wrapped
  into the async path; bodies without `cx` ignore it.
- **No-`cx` actions** `impl<G, R> Action<G, R>` (generic) so they fit any router.

## Core types

```rust
pub enum Response<O> { Unary(O), Stream(BoxStream<'static, Result<O, Error>>) }

pub struct Cx<G, R> { pub app: Arc<G>, pub req: R, /* client caller, router ref */ }
impl<G, R> Cx<G, R> {
    pub async fn call<A: Action<G, R>>(&self, p: A::Input) -> Result<A::Output>;        // typed action→action
    pub async fn client_call<C: ClientAction>(&self, p: C::Params) -> Result<C::Return>; // ws/duplex only
}

pub trait ClientAction { type Params: Model + Serialize; type Return: DeserializeOwned; const NAME: &'static str; }
```

`G` = global ctx (db pools, config; built once). `R` = request ctx (auth, images,
multipart, headers — anything; you build it per request in your handler).

## Router + handlers

```rust
let router = Router::builder()
    .ctx(app)                  // G
    .register(get_user)
    .register(chat)
    .client_action::<Notify>()
    .build();                  // R inferred from the actions
```
Handlers you call from your framework (params arrive already decoded into a
neutral `serde` value — default `serde_json::Value`):
```rust
async fn on_request(&self, req: R, action: &str, params: Value) -> Reply;            // unary
fn on_stream(&self, req: R, action: &str, params: Value) -> impl Stream<Item = Frame>; // SSE
fn on_ws(&self, req: R) -> Session;                                                  // duplex + client_call
```
`Reply = { status: "ok", data: Value } | { status: "error", error: { code, message, fields? } }`.

Dispatch: `Value → Input` (serde) → `Input.validate()` (stakit-model) → run →
`Output → Value`. Validation failure → `Reply::error` with `field_errors()`.

## TypeScript generation + sync

```rust
router.generate_ts() -> String          // typed client: per-action params/result types + kind + client-action sigs
router.generate_ts_to_path("client.ts")
```
**Sync strategy (build-verified, ts-rs/insta style):** commit the generated `.ts`
and guard it with a test so it can never drift:
```rust
#[test] fn ts_in_sync() { router().assert_ts_synced("client.ts"); }  // diff; fail if stale
//   STAKIT_UPDATE_TS=1 cargo test  → rewrite
```
Rust is the single source of truth; CI catches drift. No manual regen step.

## Crate layout & deps

```
crates/router/         (stakit-router)  deps: stakit-model, serde, serde_json, futures
  src/{lib,action,cx,router,client,error,ts,transport}.rs
crates/router-derive/  (#[action] proc-macro)  deps: syn, quote, proc-macro2
```
Core has **no** hyper/axum/tokio dependency. Streaming over `futures::Stream`;
WS/duplex framing is neutral and bridged by the user.

## Errors

Actions return **their own** error type — anything `Into<Error>`. Any
`std::error::Error` becomes a 500 via `?`; `err!(code, msg)` / `Error::new` set
explicit codes. The wire envelope is uniform: `Reply = { status, data } |
{ status, error: { code, message, fields? } }`. (`Error` deliberately does *not*
implement `std::error::Error`, so the blanket `From` has no reflexive conflict.)

## Build phasing

1. **Core + unary** — ✅ done: `Action`/erasure, `Cx<G,R>`, `Router` builder,
   `on_request`, validate+dispatch, `Error`/`Reply`, action→action; `#[action]`
   (async/sync, cx/params/none), per-action error type.
2. **TS gen** — ✅ basic (`generate_ts`). `assert_ts_synced` still TODO.
3. **Stream** — ✅ done: `#[action(stream)]`, `StreamAction`, `on_stream` →
   `'static` stream of `Frame`s.
4. **Duplex/WS** — ✅ done: `Router::session(req)` → `Session` (tokio). Pump
   inbound frames via `Session::handle(&frame)`; forward `Session::outgoing()` to
   the socket. Handles `call` frames (unary **and** stream actions, tagged by
   `id`) and `client_result` frames; `cx.client_call::<C>(params)` invokes a
   client action and awaits its reply (id→oneshot pending-map). Actions run as
   spawned tasks so a `client_call` can suspend mid-action. Requires a Tokio
   runtime; the rest of the router stays runtime-free.

One action per call (clearer than ggtype's batched map); batching can come later.
