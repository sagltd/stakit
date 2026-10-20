// Duplex socket behaviour against an in-memory mock websocket (closes
// asynchronously, like real sockets) under fake timers: auth gating, calls,
// client actions vs publish handlers, heartbeat + liveness watchdog,
// subscriptions + gap detection, reconnect + resubscribe, pause/resume.
// Wire contract: docs/transport.md.

import { afterEach, beforeEach, expect, jest, spyOn, test } from 'bun:test'

import { TransportError, createClient } from './index'
import type { ConnectOpts, WebSocketLike } from './index'

interface TestRouter {
  serverActions: {
    greet: { params: { name: string }; result: string; kind: 'unary' }
  }
  clientActions: {
    showToast: { params: { text: string }; return: string }
  }
  channels: { room: { id: string } }
}

type Sent = Record<string, unknown> & { kind: string; id?: number }

class MockSocket implements WebSocketLike {
  onopen: WebSocketLike['onopen'] = null
  onclose: WebSocketLike['onclose'] = null
  onerror: WebSocketLike['onerror'] = null
  onmessage: WebSocketLike['onmessage'] = null
  readonly sent: Sent[] = []
  closed = false

  constructor(readonly url: string) {}

  send(data: string): void {
    if (this.closed) throw new Error('InvalidStateError')
    this.sent.push(JSON.parse(data) as Sent)
  }
  /** Real sockets report the close later, not inside `close()`. */
  close(code = 1000): void {
    this.closed = true
    setTimeout(() => this.onclose?.({ code }), 0)
  }
  open(): void {
    this.onopen?.({})
  }
  receive(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) })
  }
  receiveRaw(data: unknown): void {
    this.onmessage?.({ data })
  }
  drop(code = 1006): void {
    this.closed = true
    this.onclose?.({ code })
  }
  last(kind: string): Sent {
    const frame = [...this.sent].reverse().find((f) => f.kind === kind)
    if (!frame) throw new Error(`no ${kind} frame sent`)
    return frame
  }
  count(kind: string): number {
    return this.sent.filter((f) => f.kind === kind).length
  }
}

beforeEach(() => {
  jest.useFakeTimers()
})

afterEach(() => {
  jest.useRealTimers()
  jest.restoreAllMocks()
})

/** Runs pending microtasks (async auth, promise handlers). */
const flush = async (): Promise<void> => {
  for (let i = 0; i < 10; i++) await Promise.resolve()
}

const advance = async (ms: number): Promise<void> => {
  jest.advanceTimersByTime(ms)
  await flush()
}

function connect(opts: ConnectOpts<TestRouter> = {}) {
  const sockets: MockSocket[] = []
  const errors: unknown[] = []
  const conn = createClient<TestRouter>({ url: 'http://api' }).connect({
    onHandlerError: (error) => errors.push(error),
    webSocket: (url) => {
      const socket = new MockSocket(url)
      sockets.push(socket)
      return socket
    },
    ...opts,
  })
  const current = (): MockSocket => sockets[sockets.length - 1]!
  return { conn, sockets, current, errors }
}

type Conn = ReturnType<typeof connect>['conn']

function ack(socket: MockSocket, frame: Sent, epoch = 7, seq = 0): void {
  const key = frame.key as { id: string }
  socket.receive({
    kind: 'result',
    id: frame.id,
    status: 'ok',
    data: { topic: `room:id=${key.id}`, epoch, seq },
  })
}

/** Subscribes to room `id` and acks it, collecting gaps and messages. */
function subscribed(socket: MockSocket, conn: Conn, id = 'r1') {
  const gaps: string[] = []
  const messages: unknown[] = []
  const sub = conn.subscribe(
    'room',
    { id },
    {
      onGap: (reason) => gaps.push(reason),
      onMessage: (message) => messages.push(message.params),
    },
  )
  ack(socket, socket.last('subscribe'))
  return { sub, gaps, messages }
}

const publish = (seq: number, epoch = 7, topic = 'room:id=r1') => ({
  kind: 'publish',
  topic,
  name: 'showToast',
  seq,
  epoch,
  params: { text: `m${seq}` },
})

// ── connect, auth, calls ─────────────────────────────────────────────────────

test('uses the ws url and queues calls until open', async () => {
  const { conn, current } = connect()
  expect(current().url).toBe('ws://api')
  const reply = conn.call('greet', { name: 'a' })
  expect(current().sent).toEqual([])
  current().open()
  const call = current().last('call')
  expect(call).toMatchObject({ action: 'greet', params: { name: 'a' } })
  current().receive({ kind: 'result', id: call.id, status: 'ok', data: 'hi' })
  expect(await reply).toEqual({ status: 'ok', data: 'hi' })
})

test('async auth goes first and gates calls until accepted', async () => {
  const { conn, current } = connect({
    auth: async () => ({ token: 't' }),
  })
  void conn.call('greet', { name: 'a' })
  current().open()
  await flush()
  expect(current().sent.map((f) => f.kind)).toEqual(['auth'])
  expect(current().sent[0]!.params).toEqual({ token: 't' })
  current().receive({ kind: 'result', id: current().sent[0]!.id, status: 'ok' })
  expect(current().sent.map((f) => f.kind)).toEqual(['auth', 'call'])
  expect(conn.status()).toBe('open')
})

test('401 auth closes for good', async () => {
  const codes: number[] = []
  const { conn, sockets, current } = connect({
    auth: () => 'bad',
    onAuthError: (error) => codes.push(error.code),
  })
  current().open()
  await flush()
  current().receive({
    kind: 'result',
    id: current().sent[0]!.id,
    status: 'error',
    error: { code: 401, message: 'no' },
  })
  await advance(60_000)
  expect(codes).toEqual([401])
  expect(conn.status()).toBe('closed')
  expect(sockets.length).toBe(1)
})

test('retryable auth failures and a throwing auth provider reconnect', async () => {
  let calls = 0
  const { sockets, current, errors } = connect({
    auth: () => {
      calls++
      if (calls === 2) throw new Error('token refresh failed')
      return 'token'
    },
  })
  current().open()
  await flush()
  current().receive({
    kind: 'result',
    id: current().last('auth').id,
    status: 'error',
    error: { code: 503, message: 'db blip' },
  })
  await advance(1_000)
  expect(sockets.length).toBe(2)
  current().open()
  await flush()
  expect(errors.length).toBe(1)
  await advance(1_000)
  expect(sockets.length).toBe(3)
  current().open()
  await flush()
  expect(current().count('auth')).toBe(1)
})

test('a call times out and is never sent afterwards', async () => {
  const { conn, current } = connect({ callTimeoutMs: 1_000 })
  const reply = conn.call('greet', { name: 'late' })
  await advance(1_001)
  await expect(reply).rejects.toBeInstanceOf(TransportError)
  current().open()
  expect(current().count('call')).toBe(0)
})

test('calls and sends after close reject immediately', async () => {
  const { conn } = connect()
  conn.close()
  await expect(conn.call('greet', { name: 'x' })).rejects.toBeInstanceOf(
    TransportError,
  )
  await expect(conn.send({ greet: { name: 'x' } })).rejects.toBeInstanceOf(
    TransportError,
  )
})

test('an oversized call is refused before sending', async () => {
  const { conn, current } = connect({ maxFrameBytes: 100 })
  current().open()
  const reply = conn.call('greet', { name: 'x'.repeat(200) })
  await expect(reply).rejects.toMatchObject({ code: 'frame_too_large' })
  expect(current().count('call')).toBe(0)
})

// ── client actions, publish handlers, heartbeat, frames ──────────────────────

test('answers client calls; missing, prototype and throwing handlers', async () => {
  const toasts: string[] = []
  const { current, errors } = connect({
    defineClientActions: {
      showToast: ({ text }) => {
        if (text === 'boom') throw new Error('boom')
        toasts.push(text)
        return 'shown'
      },
    },
  })
  current().open()
  const call = (id: number, name: string, text = 'a') =>
    current().receive({ kind: 'client_call', id, name, params: { text } })
  call(4, 'showToast')
  call(5, 'nope')
  call(6, 'toString')
  call(7, 'showToast', 'boom')
  await flush()
  expect(toasts).toEqual(['a'])
  const results = current().sent.filter((f) => f.kind === 'client_result')
  expect(results).toContainEqual({
    kind: 'client_result',
    id: 4,
    data: 'shown',
  })
  expect(results.find((f) => f.id === 5)).toMatchObject({
    error: { code: 404 },
  })
  expect(results.find((f) => f.id === 6)).toMatchObject({
    error: { code: 404 },
  })
  expect(results.find((f) => f.id === 7)).toMatchObject({
    error: { code: 500 },
  })
  expect(errors.length).toBe(1)
})

test('publish runs publish handlers, never client-call handlers', async () => {
  const called: string[] = []
  const { conn, current } = connect({
    defineClientActions: {
      showToast: () => {
        called.push('client_call handler')
        return 'x'
      },
    },
    publishHandlers: {
      showToast: ({ text }) => called.push(`publish ${text}`),
    },
  })
  current().open()
  subscribed(current(), conn)
  current().receive(publish(1))
  current().receive(publish(1, 1, 'user:id=u1'))
  current().receive(publish(1, 1, 'room:id=not-subscribed'))
  expect(called).toEqual(['publish m1', 'publish m1'])
})

test('answers heartbeat pings', () => {
  const { current } = connect()
  current().open()
  current().receive({ kind: 'ping' })
  expect(current().last('pong')).toEqual({ kind: 'pong' })
})

test('malformed frames are ignored', () => {
  const { conn, current } = connect()
  current().open()
  const { messages, gaps } = subscribed(current(), conn)
  current().receiveRaw('{not json')
  current().receiveRaw('null')
  current().receiveRaw(new Uint8Array([1, 2]))
  current().receive({ kind: 'publish', topic: 'room:id=r1', seq: 'one' })
  current().receive({ kind: 'reset', topic: 'room:id=r1', epoch: -1 })
  current().receive(publish(1))
  expect(messages).toEqual([{ text: 'm1' }])
  expect(gaps).toEqual([])
})

test('a quiet socket is replaced by the liveness watchdog', async () => {
  const { conn, sockets, current } = connect({ livenessTimeoutMs: 6_000 })
  current().open()
  await advance(5_000)
  current().receive({ kind: 'ping' })
  await advance(5_000)
  expect(sockets.length).toBe(1)
  await advance(3_000)
  expect(conn.status()).toBe('connecting')
  await advance(1_000)
  expect(sockets.length).toBe(2)
})

// ── subscriptions ────────────────────────────────────────────────────────────

test('delivers in order; seq jumps, epoch changes and resets report gaps', () => {
  const { conn, current } = connect()
  current().open()
  const { sub, gaps, messages } = subscribed(current(), conn)
  current().receive(publish(1))
  current().receive(publish(2))
  expect(messages.length).toBe(2)
  expect(gaps).toEqual([])
  expect(sub.topic).toBe('room:id=r1')
  current().receive(publish(4))
  current().receive(publish(1, 9))
  current().receive({ kind: 'reset', topic: 'room:id=r1', epoch: 11, seq: 0 })
  current().receive(publish(1, 11))
  expect(gaps).toEqual(['seq', 'epoch', 'reset'])
})

test('a throwing onMessage does not desync the sequence', () => {
  const { conn, current, errors } = connect()
  current().open()
  const gaps: string[] = []
  conn.subscribe(
    'room',
    { id: 'r1' },
    {
      onGap: (reason) => gaps.push(reason),
      onMessage: () => {
        throw new Error('render failed')
      },
    },
  )
  ack(current(), current().last('subscribe'))
  current().receive(publish(1))
  current().receive(publish(2))
  expect(gaps).toEqual([])
  expect(errors.length).toBe(2)
})

test('two subscriptions to one channel share the server subscription', () => {
  const { conn, current } = connect()
  current().open()
  const first: unknown[] = []
  const second: unknown[] = []
  const a = conn.subscribe(
    'room',
    { id: 'r1' },
    { onMessage: (m) => first.push(m.seq) },
  )
  const b = conn.subscribe(
    'room',
    { id: 'r1' },
    { onMessage: (m) => second.push(m.seq) },
  )
  expect(current().count('subscribe')).toBe(1)
  ack(current(), current().last('subscribe'))
  current().receive(publish(1))
  a.unsubscribe()
  expect(current().count('unsubscribe')).toBe(0)
  current().receive(publish(2))
  expect(first).toEqual([1])
  expect(second).toEqual([1, 2])
  b.unsubscribe()
  expect(current().last('unsubscribe')).toMatchObject({ topic: 'room:id=r1' })
})

test('unsubscribing before the ack releases the server subscription', () => {
  const { conn, current } = connect()
  current().open()
  const sub = conn.subscribe('room', { id: 'r1' })
  sub.unsubscribe()
  expect(current().count('unsubscribe')).toBe(0)
  ack(current(), current().last('subscribe'))
  expect(current().last('unsubscribe')).toMatchObject({ topic: 'room:id=r1' })
})

test('publishes that beat the ack are replayed after it', () => {
  const { conn, current } = connect()
  current().open()
  const messages: unknown[] = []
  const gaps: string[] = []
  conn.subscribe(
    'room',
    { id: 'r1' },
    { onMessage: (m) => messages.push(m.seq), onGap: (r) => gaps.push(r) },
  )
  const frame = current().last('subscribe')
  current().receive(publish(3))
  current().receive(publish(4))
  ack(current(), frame, 7, 3)
  expect(messages).toEqual([4])
  expect(gaps).toEqual([])
})

test('a rejected subscription reports its error', () => {
  const { conn, current } = connect()
  current().open()
  const errors: number[] = []
  conn.subscribe(
    'room',
    { id: 'secret' },
    { onError: (e) => errors.push(e.code) },
  )
  current().receive({
    kind: 'result',
    id: current().last('subscribe').id,
    status: 'error',
    error: { code: 403, message: 'forbidden' },
  })
  expect(errors).toEqual([403])
})

test('a kicked subscription is not resubscribed; a reset one is', async () => {
  const { conn, sockets, current } = connect()
  current().open()
  const kicks: string[] = []
  const kicked = subscribed(current(), conn, 'r1')
  kicked.sub.onKick = () => kicks.push('kicked')
  const kept = subscribed(current(), conn, 'r2')
  current().receive({
    kind: 'unsubscribed',
    topic: 'room:id=r1',
    reason: 'kicked',
  })
  current().receive({
    kind: 'unsubscribed',
    topic: 'room:id=r2',
    reason: 'reset',
  })
  expect(kicks).toEqual(['kicked'])
  expect(kept.gaps).toEqual(['reset'])
  expect(current().last('subscribe')).toMatchObject({ key: { id: 'r2' } })

  sockets[0]!.drop()
  await advance(1_000)
  current().open()
  const keys = current()
    .sent.filter((f) => f.kind === 'subscribe')
    .map((f) => (f.key as { id: string }).id)
  expect(keys).toEqual(['r2'])
})

// ── reconnect, pause, resume ─────────────────────────────────────────────────

test('reconnects, resubscribes, flags missed messages, keeps queued calls', async () => {
  let reconnects = 0
  const { conn, sockets, current } = connect({
    onReconnect: () => reconnects++,
  })
  current().open()
  const { gaps } = subscribed(current(), conn)
  current().receive(publish(1))
  const inflight = conn.call('greet', { name: 'sent' })

  sockets[0]!.drop()
  await expect(inflight).rejects.toBeInstanceOf(TransportError)
  const queued = conn.call('greet', { name: 'queued' })
  expect(conn.status()).toBe('connecting')
  await advance(1_000)
  expect(sockets.length).toBe(2)

  current().open()
  const resubscribe = current().last('subscribe')
  expect(resubscribe).toMatchObject({ channel: 'room', key: { id: 'r1' } })
  ack(current(), resubscribe, 7, 4)
  expect(gaps).toEqual(['resubscribe'])
  const call = current().last('call')
  expect(call).toMatchObject({ params: { name: 'queued' } })
  current().receive({ kind: 'result', id: call.id, status: 'ok', data: 'hi' })
  expect(await queued).toEqual({ status: 'ok', data: 'hi' })
  expect(reconnects).toBe(1)
})

test('a resubscribe with a new epoch reports a gap', async () => {
  const { conn, sockets, current } = connect()
  current().open()
  const { gaps } = subscribed(current(), conn)
  sockets[0]!.drop()
  await advance(1_000)
  current().open()
  ack(current(), current().last('subscribe'), 99, 0)
  expect(gaps).toEqual(['resubscribe'])
})

test('backoff doubles to the cap and resets after a healthy connection', async () => {
  spyOn(Math, 'random').mockReturnValue(0.999)
  const { sockets, current } = connect({
    minReconnectDelayMs: 100,
    maxReconnectDelayMs: 400,
  })
  const waitFor = async (ms: number, expected: number) => {
    await advance(ms - 2)
    expect(sockets.length).toBe(expected - 1)
    await advance(2)
    expect(sockets.length).toBe(expected)
  }
  current().drop()
  await waitFor(100, 2)
  current().drop()
  await waitFor(200, 3)
  current().drop()
  await waitFor(400, 4)
  current().drop()
  await waitFor(400, 5)
  current().open()
  await advance(5_000)
  current().drop()
  await waitFor(100, 6)
})

test('slow-consumer and cap closes back off at least 5s', async () => {
  const { sockets, current } = connect()
  current().open()
  current().drop(4029)
  await advance(4_999)
  expect(sockets.length).toBe(1)
  await advance(30_000)
  expect(sockets.length).toBe(2)
})

test('terminal close codes and reconnect: false stop reconnecting', async () => {
  for (const code of [4003, 4010, 4011]) {
    const { conn, sockets, current } = connect()
    current().open()
    current().drop(code)
    await advance(60_000)
    expect(sockets.length).toBe(1)
    expect(conn.status()).toBe('closed')
  }
  const once = connect({ reconnect: false })
  once.current().open()
  once.current().drop()
  await advance(60_000)
  expect(once.sockets.length).toBe(1)
})

test('pause closes without reconnecting; resume resubscribes', async () => {
  const statuses: string[] = []
  const { conn, sockets, current } = connect({
    onStatus: (status) => statuses.push(status),
  })
  current().open()
  subscribed(current(), conn)
  conn.pause()
  expect(sockets[0]!.closed).toBe(true)
  await advance(60_000)
  expect(sockets.length).toBe(1)
  expect(conn.status()).toBe('paused')

  conn.resume()
  current().open()
  expect(current().last('subscribe')).toMatchObject({ key: { id: 'r1' } })
  expect(statuses).toEqual(['open', 'paused', 'connecting', 'open'])
})

test('a stale onopen after pause is ignored', async () => {
  const { conn, sockets } = connect()
  conn.pause()
  sockets[0]!.open()
  expect(conn.status()).toBe('paused')
})

test('resume skips the backoff wait and replaces a quiet open socket', async () => {
  const { conn, sockets, current } = connect({
    minReconnectDelayMs: 10_000,
    maxReconnectDelayMs: 10_000,
    livenessTimeoutMs: 60_000,
  })
  current().drop()
  expect(sockets.length).toBe(1)
  conn.resume()
  expect(sockets.length).toBe(2)
  current().open()
  conn.resume()
  expect(sockets.length).toBe(2)
  await advance(31_000)
  conn.resume()
  expect(sockets.length).toBe(3)
})

// ── legacy send / stream ─────────────────────────────────────────────────────

test('legacy stream: unary results are final, stream items until end', async () => {
  const { conn, current } = connect()
  current().open()
  await conn.send({ greet: { name: 'a' } })
  const call = current().last('call')
  current().receive({ kind: 'result', id: call.id, status: 'ok', data: 'hi' })
  current().receive({ kind: 'result', id: call.id, status: 'ok', data: 'dup' })
  expect((await conn.stream.next()).value).toEqual({
    greet: { status: 'ok', data: 'hi' },
  })
  await conn.send({ greet: { name: 'b' } })
  const stream = current().last('call')
  current().receive({
    kind: 'result',
    id: stream.id,
    status: 'ok',
    data: 1,
    more: true,
  })
  current().receive({
    kind: 'result',
    id: stream.id,
    status: 'ok',
    data: 2,
    more: true,
  })
  current().receive({ kind: 'end', id: stream.id })
  expect((await conn.stream.next()).value).toEqual({
    greet: { status: 'ok', data: 1 },
  })
  expect((await conn.stream.next()).value).toEqual({
    greet: { status: 'ok', data: 2 },
  })
  conn.close()
  expect((await conn.stream.next()).done).toBe(true)
})

test('legacy stream reports a lost connection', async () => {
  const { conn, current } = connect()
  current().open()
  await conn.send({ greet: { name: 'a' } })
  current().drop()
  const result = (await conn.stream.next()).value as {
    greet: { status: string; error: { type: string } }
  }
  expect(result.greet.status).toBe('error')
  expect(result.greet.error.type).toBe('connection_lost')
})
