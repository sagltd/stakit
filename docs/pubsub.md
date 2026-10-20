# Pub/sub + realtime sessions

Two server→client primitives over a duplex session (wire frames in
`transport.md`):

- **`cx.publish::<C>(&channel, &params)`** — fire-and-forget fan-out of client
  action `C` to every connection subscribed to `channel`, on every node.
- **`cx.client_call::<C>(params)`** (this connection) /
  **`cx.client_call_device::<C>(user, device, params)`** (the one connection
  holding `user`'s device, on any node) — request/response with timeout.

Both work from any transport: an HTTP action can notify websocket clients.

## Channels

```rust
#[derive(Model, Serialize, Deserialize)]
struct Room { id: String }

impl Channel for Room { const NAME: &'static str = "room"; }           // publish side

impl Authorize<App, Req> for Room {                                    // subscribe side
    async fn authorize(&self, cx: &Cx<App, Req>) -> Result<bool, Error> {
        is_member(&cx.app.db, cx.req.user_id, &self.id).await
    }
}

Router::builder().channel::<Room>() /* clients may now subscribe("room", { id }) */
```

- Topic = `NAME:field=value:…` — fields in declaration order (read through an
  order-preserving map, independent of serde_json's `preserve_order`),
  numbers/booleans tagged `#` (`"7"` ≠ `7`), field names included (skipped
  optional fields can't collide), `% : = #` escaped, ≤ 256 bytes. Keys are
  validated (`Model`) and `authorize`d on every subscribe (`false` → `403`).
- Built-ins (names `user` / `device` are reserved): an authenticated session
  is auto-subscribed to `user:id=<user>` and, with a device,
  `device:user=<user>:id=<device>`.
- **Revocation:** `hub.kick_user(topic, user)` (membership change),
  `hub.close_user(user)` / `hub.close_device(user, device)` (logout, deletion,
  token revoked) — local first, then every node. A kick that lands while a
  subscribe's `authorize` runs moves the topic's revocation mark; the
  subscribe is refused and re-authorized (up to 3 times).
- Server-side subscribe (trusted, no cap, kept across resets):
  `session.subscribe(topic)`.
- No `#[channel]` macro: the manual impl is two short blocks, and `authorize`
  is hand-written anyway.

## Devices

A device belongs to a user and is held by **one connection**: another user's
claim is `409` (`device_conflict`); the same user's newer connection
supersedes the old one (closed `4010`, also on other nodes). The auth hook must
*prove* ownership (e.g. a signed device token). `client_call_device` addresses
the device through its owner, so an action can't be steered into another
user's device; nodes without the holder stay silent.

## Sessions

`Router::session(req)` → `Session`. The adapter feeds `handle_bytes`, forwards
`outgoing()` (pre-serialized JSON text, `Bytes`) to the socket, and when
`closed()` resolves flushes the queue and closes with `CloseReason::code()`.
It should also cap the socket's message size at `max_frame_bytes`, check
`Origin` when cookies can authenticate, and cap pre-auth connections per IP
(see `examples/axum-server`). Keep `R` cheap to clone (e.g. an `Arc`): every
call and subscribe clones it.

| Knob (`SessionConfig`)     | Default | Behaviour                                    |
| -------------------------- | ------- | -------------------------------------------- |
| `heartbeat`                | 25s     | server `ping` (+≤10% jitter); client `pong`  |
| `idle_timeout`             | 60s     | no inbound frame → close `4008`              |
| `auth_window`              | 5s      | with `on_auth`, no `auth` frame → `4001`     |
| `max_connections_per_user` | 16      | extra session → `429` + close `4029`         |
| `max_frame_bytes`          | 64 KiB  | bigger inbound frame → close `1009`          |
| `max_frames_per_second`    | 100     | more inbound frames → close `1008`           |
| `max_subscriptions`        | 128     | client subscribes beyond → `429`             |
| `outgoing_buffer`          | 1024    | full → close `4009` (slow client); counts frames, not bytes |

`Builder::on_auth(|app, req, params| async { Ok((req, identity)) })`
authenticates the `auth` frame — one at a time; a duplicate gets `409` and
never closes an authenticated session. Adapters that authenticate at upgrade
call `session.authenticate(req, &identity)` instead (all or nothing).

## Guarantees

- **At-most-once.** Nothing is persisted or replayed. Treat pub/sub as an
  invalidation signal; the database-backed API is the source of truth.
- **Gap detection.** Each topic on a node has a monotonic `seq` and an
  `epoch`. The subscribe ack is queued under the hub's lock, so no publish
  overtakes it, and carries the current `(epoch, seq)`. A `seq` jump, a new
  `epoch`, a `reset` frame, or a resubscribe ack that moved while
  disconnected → the TS client's `onGap`: refetch.
- **Broker loss.** When the broker loses messages (Postgres listener drop),
  client-requested subscriptions are dropped (`unsubscribed`, reason `reset`)
  so the client resubscribes through `authorize` again — a kick missed during
  the gap can't leave a revoked member listening — and auto channels get a
  `reset` frame. Auto channels have no client-side subscription, hence no
  `onGap`: resync them in `onReconnect`.
- **Order.** Per topic per node, delivery order is `seq` order (assigned and
  sent under one lock). A publish is delivered on its own node first, then
  handed to the broker; other nodes receive one node's publishes in that
  node's order, but publishes from different nodes interleave arbitrarily —
  two nodes can see two concurrent publishes in opposite orders.
- **Failure.** A payload over the broker's limit (`413`) reaches nobody. A
  broker hand-off failure is returned *after* local delivery (other nodes may
  have missed it). NOTIFY is sent outside the caller's transaction: publish
  after commit.
- **Slow consumers are closed, never waited on** (publishes, replies, kicks).
  Stream-action results share the queue with backpressure, so a client that
  can't drain a stream can be closed by the next publish.

## Multi-node: `Broker`

A `Hub` (one per node) holds the topic → subscriber index (64 `RwLock`
shards, hashbrown) and a `Broker`:

```rust
trait Broker {
    fn attach(&self, sink: BrokerSink);          // inbound → hub
    fn subscribe(&self, topic: &str);            // first local subscriber
    fn unsubscribe(&self, topic: &str);          // last local subscriber left
    fn validate(&self, topic, payload) -> Result<()>;   // size limit, before delivery
    fn publish(&self, topic, payload) -> BoxFuture<Result<()>>;
    fn call(&self, topic, payload, timeout) -> BoxFuture<Result<Value>>;
}
```

- **`InMemoryBroker`** — single node (the default), or `link()` several hubs in
  one process to simulate a cluster. It knows every node, so a device call to
  nobody is `404`.
- **`stakit-pubsub-postgres`** — `PostgresBroker::connect(pool, &secret)`:
  - one `LISTEN` connection per node on `stakit_pubsub`; every node receives
    every notification, and the hub drops topics it doesn't hold before
    parsing the payload;
  - one publisher task drains a bounded queue (4096; full → `503`) with a
    single `select pg_notify($1, p) from unnest($2::text[]) p` per batch of up
    to 256, in order;
  - every envelope is HMAC-SHA256-signed (secret ≥ 32 bytes, shared by the
    nodes), carries a per-node counter and a timestamp; unsigned, forged,
    replayed (counter not increasing) or stale (> 30s skew) envelopes are
    dropped, so `NOTIFY` access alone can't inject publishes, kicks, calls
    or replies;
  - **payloads must stay under 8000 bytes** (Postgres limit; envelope
    included) — bigger publishes fail with `413` before anyone gets them:
    publish ids, fetch the data;
  - a dropped listener resets local subscribers, reconnects with backoff
    (250ms → 30s) and resets again;
  - device calls: the holder's node answers, the others stay silent; nothing
    held anywhere → the caller times out (`504`). At most 64 inbound calls run
    per node; extra ones are dropped (the caller times out).
- **Later: NATS** (or Redis) — implement `Broker` with subject-per-topic
  subscriptions driven by `subscribe`/`unsubscribe` and request/reply for
  `call`; nothing above the trait changes.

## Measurements

Apple Silicon laptop under memory pressure; `cargo bench -p stakit-router
--bench pubsub` (divan, median, release). Multi-thread runtime (4 workers),
every subscriber has a parked reader task, queues are drained continuously,
16 topics, 500 B payloads; a fan-out is timed until the **last** subscriber
has the frame.

| Benchmark                                            | Median   |
| ---------------------------------------------------- | -------- |
| publish fan-out, 1 subscriber                        | 8.6 µs   |
| publish fan-out, 100 subscribers                     | 18.3 µs  |
| publish fan-out, 1 000 subscribers                   | 174 µs   |
| publish fan-out, 10 000 subscribers                  | 5.0 ms   |
| 16 concurrent publishers × 100 subscribers each      | 116 µs   |
| subscribe + unsubscribe (new topic each time)        | 192 ns   |
| subscribe + unsubscribe on a 1 000-subscriber topic  | 60 ns    |

The 10 000-subscriber case is dominated by waking 10 000 reader tasks across
threads (~0.5 µs each), not by the hub. `--bench pubsub_alloc`: one 500 B
publish to 100 subscribers allocates 6 times (1.3 KB) regardless of the
subscriber count; opening an authenticated session allocates 61 times
(5.7 KB).

Memory per idle session (router layer, no socket or its buffers), from
`tests/session_memory.rs`: 10 000 authenticated sessions (user + device
channels, an `Arc`'d request context with four headers, one more
subscription), runtime driven → **≈ 5.4 KiB each**. The test asserts
**12 KiB** — about 2×, so allocator noise passes and a doubled footprint
fails. The outgoing queue's first block (32 slots), the timer task, three
topic memberships and the hub/client-call entries make up most of it; a real
connection adds the socket and TLS buffers (tens of KiB).
