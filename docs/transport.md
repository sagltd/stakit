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
`action → envelope`. One round-trip, many actions. Params **always** live in the
`q` query value (JSON-encoded), so the server reads them the same way for every
method; the body is used only for files.

- **No files:** `GET {url}?q=<json>` (or `POST {url}?q=<json>` with no body).
- **With files:** `POST {url}?q=<json>` `Content-Type: multipart/form-data`,
  params still in `?q=`, each file a repeated `file` part.

Response: HTTP **200 always** (per-action errors live in the envelope), body:

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

`POST {streamUrl}?q=<json>`. Response is newline-delimited JSON frames, one per
line. The action is implied by the request, so frames carry only `type`:

```jsonc
{ "type": "next",  "data": <value> }
{ "type": "error", "error": { … } }   // terminates the stream
{ "type": "end" }                     // normal completion
```

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

- **HTTP:** multipart `file` parts (repeatable) + params in `?q=`.
- **stream / ws:** out of scope for v1 (send file refs in params; upload over
  HTTP multipart first). May add `upload_file` frames later.

## Per-call overrides

Both clients accept optional per-call options. They affect **only that call**;
the client's base url/headers are never mutated.

- `url` — overrides base url for this call (fan-out to other servers).
- `headers` — **merged over** base headers (per-call wins on key clash).
- `files` — multipart upload (HTTP only).
