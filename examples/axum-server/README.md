# axum-server-example

Full [axum](https://docs.rs/axum) integration for `stakit-router`: HTTP unary,
SSE streaming, and a WebSocket duplex endpoint with server→client `client_call`.

It's a standalone crate (excluded from the workspace), so run it from here:

```bash
cd examples/axum-server
cargo run        # listening on http://127.0.0.1:3007
```

## Try it

```bash
# unary (200)
curl -s localhost:3007/rpc/greet -H 'content-type: application/json' -H 'x-admin: true' \
  -d '{"name":"bob"}'
# -> {"status":"ok","data":{"message":"Hello, bob! (admin=true)"}}

# validation error (422)
curl -s localhost:3007/rpc/greet -H 'content-type: application/json' -d '{"name":""}'
# -> {"status":"error","error":{"code":422,"type":"validation","message":"validation failed","fields":{"name":[...]}}}

# unknown action (404)
curl -s localhost:3007/rpc/nope -H 'content-type: application/json' -d 'null'

# SSE stream
curl -sN localhost:3007/sse/count -H 'content-type: application/json' -d '{"n":3}'
# -> data: {"type":"next","data":0}  ... data: {"type":"end"}
```

## WebSocket (duplex)

Connect to `ws://127.0.0.1:3007/ws` and send a call frame:

```json
{ "kind": "call", "id": 1, "action": "greet", "params": { "name": "ada" } }
```

You'll receive `{ "kind": "result", "id": 1, "status": "ok", "data": { … } }`.
Stream actions over the same socket emit multiple `result` frames then an `end`
frame. Actions that use `cx.client_call::<C>(…)` send a `client_call` frame the
client answers with `{ "kind": "client_result", "id": …, "data": … }`.

## How it wires up

The router lives in axum state as `Arc<Router<App, Auth>>`. Each handler builds
the request context (`Auth`) from headers and calls `on_request` / `on_stream` /
`session` — the router never touches sockets or JSON itself.
