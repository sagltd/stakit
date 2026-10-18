# axum-server-example

Full [axum](https://docs.rs/axum) integration for `stakit-router`: HTTP unary,
JSONL streaming, and a WebSocket duplex endpoint with server→client
`client_call`. Payload-routed: the action name lives in the JSON body, never
the URL (see `docs/transport.md`).

It's a standalone crate (excluded from the workspace), so run it from here:

```bash
cd examples/axum-server
cargo run        # listening on http://127.0.0.1:3007
```

## Try it

```bash
# unary (200; per-action outcome in the envelope)
curl -s localhost:3007/app -H 'content-type: application/json' -H 'x-admin: true' \
  -d '{"greet":{"name":"bob"}}'
# -> {"greet":{"status":"ok","data":{"message":"Hello, bob! (admin=true)"}}}

# validation error (422 inside the envelope)
curl -s localhost:3007/app -H 'content-type: application/json' -d '{"greet":{"name":""}}'
# -> {"greet":{"status":"error","error":{"code":422,"type":"validation",...}}}

# upload: params in the multipart `q` field, bytes in `file` parts
curl -s localhost:3007/app -F 'q={"save_image":{"fileName":"a.png"}}' -F 'file=@a.png'

# JSONL stream
curl -sN localhost:3007/stream -H 'content-type: application/json' -d '{"count":{"n":3}}'
# -> {"type":"next","index":0,"action":"count","data":0} ... {"type":"end",...}
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
