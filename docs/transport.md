# Transport contract

The wire protocol every stakit client (TypeScript, Rust) and every server
adapter (axum, …) **must** follow. Clients pick a transport; the server obeys
the same shapes. Frozen here so the two client implementations stay in lockstep.

## Envelope

Every action outcome is one of:

```jsonc
{ "status": "ok",    "data":  <value> }
{ "status": "error", "error": { "code": 422, "message": "…", "fields"?: { "name": ["…"] } } }
```

`fields` is present only for validation errors. `code` mirrors the HTTP-style
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
never as an envelope. Body:

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

Clients skip blank lines, malformed lines and frames of any **unknown** `type`
(forward compatible: servers send `heartbeat` frames so proxies do not idle the
connection out). Only `error` and `end` terminate a stream. A non-2xx response
status is a transport error.

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
{ "kind": "end",    "id": 1 }                                     // stream action done
{ "kind": "client_call", "id": 7, "name": "showToast", "params": <value> }   // server invokes client
```

`id` is a per-connection monotonic integer chosen by whichever side initiates.

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
