# Transport contract

The wire protocol every stakit client (TypeScript, Rust) and every server
adapter (axum, …) **must** follow. Clients pick a transport; the server obeys
the same shapes. Frozen here so the two client implementations stay in lockstep.

## Envelope

Every action outcome is one of:

```jsonc
{ "status": "ok",    "data":  <value> }
{ "status": "error", "error": { "code": 422, "type"?: "validation", "message": "…", "fields"?: { "name": ["…"] }, "data"?: <value> } }
```

`type` is the stable, machine-readable error code clients branch on (e.g.
`budget_exceeded`); it is omitted when it is the default, `error`.
`fields` is present only for validation errors. `data` is present only when the
action attached a client-facing payload with `Error::with_data` (for example
when a quota resets), so clients can render their own copy. `code` mirrors the HTTP-style
status of the individual action (not the transport response).

## HTTP — unary (batch)

Request carries a **map** of `action → params`; response is a **map** of
`action → envelope`. One round-trip, many actions.

**Params never ride in the URL of a POST** (CWE-598: URLs — and anything in
them, like credentials or one-time codes — end up in access logs, proxies and
browser history). Clients default to POST:

- **No files (default):** `POST {url}`, `Content-Type: application/json`, body =
  the JSON payload.
- **With files:** `POST {url}`, `Content-Type: multipart/form-data`; the JSON
  payload is the **`q` form field**, each file a repeated **`file`** part.
  Nothing goes in the query string.
- **Explicit GET (opt-in, `method: GET`):** `GET {url}?q=<json>`, no body. For
  cacheable, non-sensitive reads only; a server may choose not to serve it.
  GET cannot carry files.

Response: HTTP **200 always** for a dispatched payload (per-action errors live
in the envelope). A non-2xx status is a transport-level failure (rate limited,
body too large, malformed request) and clients surface it as a transport error,
never as an envelope, keeping its status, the `x-error-code` header and
`Retry-After` (the TypeScript client throws a `TransportError`). Body:

```jsonc
{ "greet": { "status": "ok", "data": { "message": "hi" } },
  "count": { "status": "error", "error": { "code": 422, "message": "…" } } }
```

### Payload shape — object or array

The action name is **in the payload**, never the URL. A payload is either:

- **object** — `{ "greet": {…}, "count": {…} }` (keyed; response is an object of
  envelopes). Most common.
- **array** — `[["greet", {…}], ["greet", {…}]]` (ordered, duplicates allowed;
  response is an array of envelopes in the same order).

The router resolves each entry to its action via an O(1) name lookup, so one
endpoint serves the whole API. This shape is identical across HTTP and stream.

## HTTP — stream (JSONL)

`POST {streamUrl}`, `Content-Type: application/json`, body = the JSON payload
(one stream action, e.g. `{ "count": { "n": 3 } }`). An explicit GET puts it in
`?q=` instead, like unary. Response is newline-delimited JSON frames
(`application/x-ndjson`), one per line. The action is implied by the request, so
clients only need `type` (servers may add `index` / `action`):

```jsonc
{ "type": "next",  "data": <value> }
{ "type": "error", "error": { … } }   // terminates the stream
{ "type": "end" }                     // normal completion
{ "type": "heartbeat" }               // keep-alive while idle — ignore it
```

Clients skip blank lines and frames of any **unknown** `type`
(forward compatible: servers send `heartbeat` frames so proxies do not idle the
connection out). Only `error` and `end` terminate a stream. A non-2xx response
status is a transport error. The last line may omit its newline. A body that
breaks off or ends before an `end`/`error` frame is **not** a clean finish:
clients report it (`stream_interrupted`), and a `next` frame whose `data` does
not decode is an error, not a skip. Malformed nonempty JSON lines, missing frame
types, and invalid known `next` / `error` / `end` fields are transport failures
(`stream_invalid_frame`), never silently discarded. `next` requires `data`;
`error` requires an error object with numeric `code` and string `message`;
`end` carries neither `data` nor `error`. Error metadata, when present, must
match the declared error-body types.

## WebSocket / duplex

Bidirectional frames. Client → server:

```jsonc
{ "kind": "call",          "id": 1, "action": "progress", "params": <value> }
{ "kind": "client_result", "id": 7, "data": <value> }            // reply to a server→client call
{ "kind": "client_result", "id": 7, "error": { … } }
```

Server → client:

```jsonc
{ "kind": "result", "id": 1, "status": "ok",    "data":  <value> }
{ "kind": "result", "id": 1, "status": "error", "error": { … } }
{ "kind": "result", "id": 1, "status": "ok", "data": <item>, "more": true } // stream item
{ "kind": "end",    "id": 1 }                                     // stream action done
{ "kind": "client_call", "id": 7, "name": "showToast", "params": <value> }   // server invokes client
```

`id` is a per-connection monotonic integer chosen by whichever side initiates.

### Session lifecycle + pub/sub frames

Client → server (each request frame is answered by a `result` with its `id`):

```jsonc
{ "kind": "auth",        "id": 1, "params": <value> }                  // → result ok | error (then close)
{ "kind": "subscribe",   "id": 2, "channel": "room", "key": { "id": "r1" } }
                                   // → result { "topic": "room:id=r1", "epoch": 81723, "seq": 4 }
{ "kind": "unsubscribe", "id": 3, "topic": "room:id=r1" }             // → result true | false
{ "kind": "pong" }                                                    // heartbeat reply
```

Server → client:

```jsonc
{ "kind": "ping" }                                                    // every 25s; answer `pong`
{ "kind": "publish", "topic": "room:id=r1", "name": "message", "seq": 5, "epoch": 81723, "params": <value> }
{ "kind": "reset", "topic": "user:id=u1", "epoch": 90211, "seq": 0 }   // history lost: resync
{ "kind": "unsubscribed", "topic": "room:id=r1", "reason": "kicked" }  // removed; "reset": resubscribe
```

- When the server installs an auth hook, a session must send `auth` within 5s;
  until it succeeds `call` / `subscribe` / `unsubscribe` get a `401` result.
- The subscribe ack is always sent before any publish on that topic.
- `publish.name` is a client action name: clients run their *publish*
  handler for it (never the `client_call` handler) and the subscription's
  message hook.
- `unsubscribed` with reason `reset`: the server lost broker messages and
  dropped the subscription for re-authorization — subscribe again.
- `seq` increments by one per publish on a topic; a jump, or a different
  `epoch`, means messages were missed. Both stay below 2^53 (exact in JS).
- Inbound frames are capped at 64 KiB. Any inbound frame counts as liveness;
  a session with none for 60s is closed.
- A second `auth` while one is in flight or after success gets `409` and
  never closes the session.
- Client error replies (`client_result` with `error`) are clamped to a
  4xx/5xx code and a 256-character message.
- Server-initiated closes use these websocket close codes: `4001` auth timeout,
  `4003` unauthorized, `4010` superseded (the device connected again),
  `4011` revoked — do not reconnect after these three; `4029` too many
  connections, `4009` slow consumer, `1008` rate limited (reconnect after a
  backoff of at least 5s); `4008` idle; `1009` frame too large. Inbound
  frames are limited to 100 per second.

## Files

- **HTTP:** multipart `file` parts (repeatable) + params in the `q` form field
  (never in the URL).
- **stream / ws:** out of scope for v1 (send file refs in params; upload over
  HTTP multipart first). May add `upload_file` frames later.

## Per-call overrides

Both clients accept optional per-call options. They affect **only that call**;
the client's base url/headers are never mutated.

- `url` — overrides base url for this call (fan-out to other servers).
- `headers` — **merged over** base headers (per-call wins on key clash).
- `files` — multipart upload (HTTP only).
- `method` — defaults to `POST` (JSON body); `GET` opts into `?q=`.

Clients may also accept a transport hook at construction — the TypeScript
client takes an injectable `fetch` (e.g. `expo/fetch` on React Native, whose
responses expose a readable body for streaming); the Rust client takes a custom
`reqwest` pool. The wire format is identical either way.
