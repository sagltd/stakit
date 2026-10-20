// The duplex websocket connection: auth, calls, server→client client actions,
// typed pub/sub subscriptions, heartbeat replies + a liveness watchdog, and
// reconnect with backoff + resubscribe. Wire contract: docs/transport.md,
// semantics: docs/pubsub.md.

import { readFrame } from './socket-frames'
import type { Frame } from './socket-frames'
import { createSubscriptions } from './socket-subscriptions'
import type { Subscription, SubscriptionHandlers } from './socket-subscriptions'
import { TransportError } from './transport-error'
import type {
  ActionResult,
  CallOpts,
  ChannelsOf,
  ErrorBody,
  ParamsMap,
  PublishHandlers,
  ResultMap,
  ServerActionsOf,
} from './types'

export type {
  GapReason,
  PublishMessage,
  Subscription,
} from './socket-subscriptions'

/** The parts of a `WebSocket` the client uses (inject one in tests / RN). */
export interface WebSocketLike {
  onopen: ((event: unknown) => void) | null
  onclose: ((event: { code?: number; reason?: string }) => void) | null
  onerror: ((event: unknown) => void) | null
  onmessage: ((event: { data: unknown }) => void) | null
  send(data: string): void
  close(code?: number, reason?: string): void
}

/** Opens a websocket to `url` (defaults to the global `WebSocket`). */
export type WebSocketFactory = (url: string) => WebSocketLike

export type SocketStatus = 'connecting' | 'open' | 'paused' | 'closed'

/** Options for `client.connect`. */
export interface ConnectOpts<R> extends Pick<
  CallOpts<R>,
  'url' | 'defineClientActions'
> {
  /**
   * Credentials sent as the first (`auth`) frame on every (re)connect; may be
   * async (e.g. refresh a token). Calls and subscriptions wait for it. A
   * `401`/`403` answer closes for good; other failures retry with backoff.
   */
  readonly auth?: () => unknown
  /**
   * Handlers for `publish` frames, by client action name (separate from
   * `defineClientActions`, which only answer the server's `client_call`s).
   */
  readonly publishHandlers?: Partial<PublishHandlers<R>>
  /** Reconnect with backoff after an unexpected close (default `true`). */
  readonly reconnect?: boolean
  /** First reconnect delay cap (default 250ms), doubling up to the max. */
  readonly minReconnectDelayMs?: number
  /** Longest reconnect delay (default 30s). */
  readonly maxReconnectDelayMs?: number
  /** A `call` without a reply after this long rejects (default 30s). */
  readonly callTimeoutMs?: number
  /** No frame (not even a ping) for this long → reconnect (default 60s). */
  readonly livenessTimeoutMs?: number
  /** Bigger outgoing frames are refused before sending (default 64 KiB). */
  readonly maxFrameBytes?: number
  /** Called whenever the connection status changes. */
  readonly onStatus?: (status: SocketStatus) => void
  /** Called after a reconnect: resync state the subscriptions don't cover. */
  readonly onReconnect?: () => void
  /** Called when auth is rejected (`401`/`403`; the socket stays closed). */
  readonly onAuthError?: (error: ErrorBody) => void
  /** A user callback or handler threw (default: `console.error`). */
  readonly onHandlerError?: (error: unknown) => void
  /** Custom websocket constructor (tests, React Native polyfills). */
  readonly webSocket?: WebSocketFactory
}

/** A live duplex websocket connection. */
export interface Connection<R> {
  /** Invokes server actions (one `call` frame per entry in the map). */
  readonly send: <P extends ParamsMap<R>>(params: P) => Promise<void>
  /** Async stream of result maps for `send` calls, until `close()`. */
  readonly stream: AsyncGenerator<ResultMap<R, ParamsMap<R>>>
  /**
   * Calls one unary action. Queued while disconnected; rejects on timeout,
   * on `close()`, or if the connection drops after it was sent.
   */
  readonly call: <K extends keyof ServerActionsOf<R> & string>(
    action: K,
    params: ServerActionsOf<R>[K] extends { params: infer P } ? P : never,
  ) => Promise<
    ActionResult<ServerActionsOf<R>[K] extends { result: infer T } ? T : never>
  >
  /** Subscribes to a channel instance; resubscribed after every reconnect. */
  readonly subscribe: <K extends keyof ChannelsOf<R> & string>(
    channel: K,
    key: ChannelsOf<R>[K],
    handlers?: SubscriptionHandlers,
  ) => Subscription
  /** Closes the socket without forgetting subscriptions (app backgrounded). */
  readonly pause: () => void
  /**
   * Network/foreground hint: reconnects now if paused, waiting on backoff, or
   * if the socket has gone quiet (probably dead after a background period).
   */
  readonly resume: () => void
  /** The current connection status. */
  readonly status: () => SocketStatus
  /** Closes the connection for good; pending calls reject. */
  readonly close: () => void
}

/**
 * Pending reply for a request id: `once` waiters take a single `result`;
 * stream calls (legacy `send`) keep theirs until a final result or `end`.
 */
interface Waiter {
  readonly onFrame: (frame: Frame) => void
  readonly once?: boolean
  readonly lost?: (error: TransportError) => void
}

/** Close codes after which reconnecting cannot help. */
const TERMINAL_CLOSE_CODES = new Set([4003, 4010, 4011])
/** The server is shedding us: back off at least 5s. */
const SLOW_CLOSE_CODES = new Set([4009, 4029, 1008])
/** A connection this old resets the backoff. */
const HEALTHY_AFTER_MS = 5_000
/** Legacy `stream` results kept for an absent reader. */
const MAX_STREAM_QUEUE = 1024

interface SocketInput<R> {
  readonly url: string
  readonly handlers: ReadonlyMap<string, (params: unknown) => unknown>
  readonly publishHandlers: ReadonlyMap<string, (params: unknown) => unknown>
  readonly opts: ConnectOpts<R>
}

const defaultFactory: WebSocketFactory = (url) =>
  new WebSocket(url) as unknown as WebSocketLike

const transportError = (message: string, code: string): TransportError =>
  new TransportError(message, { status: 0, code })

const toResult = (frame: Frame): unknown =>
  frame.status === 'error'
    ? { status: 'error', error: frame.error }
    : { status: 'ok', data: frame.data }

/** Opens a reconnecting duplex connection. */
export function connectSocket<R>(input: SocketInput<R>): Connection<R> {
  const { url, handlers, publishHandlers, opts } = input
  const factory = opts.webSocket ?? defaultFactory
  const minDelay = opts.minReconnectDelayMs ?? 250
  const maxDelay = opts.maxReconnectDelayMs ?? 30_000
  const callTimeout = opts.callTimeoutMs ?? 30_000
  const liveness = opts.livenessTimeoutMs ?? 60_000
  const maxFrame = opts.maxFrameBytes ?? 64 * 1024
  const report =
    opts.onHandlerError ??
    ((error: unknown) => console.error('stakit socket handler failed', error))

  let socket: WebSocketLike | null = null
  let status: SocketStatus = 'connecting'
  let ready = false
  let everReady = false
  let attempt = 0
  let lastInbound = Date.now()
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined
  let healthyTimer: ReturnType<typeof setTimeout> | undefined
  let watchdog: ReturnType<typeof setInterval> | undefined
  let nextId = 1
  const outbox = new Map<number, string>()
  const waiters = new Map<number, Waiter>()
  const queue: ResultMap<R, ParamsMap<R>>[] = []
  let wake: (() => void) | null = null

  const notify = (): void => {
    wake?.()
    wake = null
  }

  /** Runs user code; errors (sync or async) go to `onHandlerError`. */
  const safe = (run: () => unknown): void => {
    try {
      const out = run()
      if (out instanceof Promise) out.catch(report)
    } catch (error) {
      report(error)
    }
  }

  const setStatus = (next: SocketStatus): void => {
    if (status === next) return
    status = next
    safe(() => opts.onStatus?.(next))
  }

  /** Sends now; a closing socket's throw is swallowed (its close follows). */
  const write = (frame: object): void => {
    try {
      socket?.send(JSON.stringify(frame))
    } catch {
      // the socket is closing; onclose handles it
    }
  }

  const request = (kind: string, fields: object, waiter: Waiter): number => {
    const id = nextId++
    waiters.set(id, waiter)
    write({ kind, id, ...fields })
    return id
  }

  const subscriptions = createSubscriptions({
    isReady: () => ready,
    subscribe: (channel, key, onFrame) => {
      request('subscribe', { channel, key }, { once: true, onFrame })
    },
    unsubscribe: (topic) => write({ kind: 'unsubscribe', id: nextId++, topic }),
    safe,
    delivered: (frame) => {
      const handler = publishHandlers.get(frame.name ?? '')
      if (handler) safe(() => handler(frame.params))
    },
  })

  /** Fails replies that were sent on the lost socket (queued ones stay). */
  const failSent = (error: TransportError): void => {
    for (const [id, waiter] of waiters) {
      if (outbox.has(id)) continue
      waiters.delete(id)
      waiter.lost?.(error)
    }
  }

  const detach = (current: WebSocketLike | null): void => {
    if (!current) return
    current.onopen = null
    current.onmessage = null
    current.onerror = null
    current.onclose = null
  }

  /** Drops the current socket and decides what happens next. */
  const lost = (code: number | undefined): void => {
    const current = socket
    socket = null
    ready = false
    clearTimeout(healthyTimer)
    clearInterval(watchdog)
    detach(current)
    try {
      current?.close(1000)
    } catch {
      // already closed
    }
    failSent(
      transportError('connection lost before the reply', 'connection_lost'),
    )
    subscriptions.disconnected()
    if (status === 'paused' || status === 'closed') return
    if (opts.reconnect === false || TERMINAL_CLOSE_CODES.has(code ?? 0))
      return stop()
    scheduleReconnect(code)
  }

  const scheduleReconnect = (code: number | undefined): void => {
    setStatus('connecting')
    const cap = Math.min(maxDelay, minDelay * 2 ** attempt)
    attempt++
    // Full jitter spreads a herd of clients after a server restart; a server
    // shedding load (slow consumer, connection cap) gets at least 5s.
    const delay = Math.random() * cap
    const shed = SLOW_CLOSE_CODES.has(code ?? 0)
    reconnectTimer = setTimeout(open, shed ? Math.max(5_000, delay) : delay)
  }

  const becomeReady = (): void => {
    ready = true
    setStatus('open')
    healthyTimer = setTimeout(() => {
      attempt = 0
    }, HEALTHY_AFTER_MS)
    subscriptions.resubscribe()
    for (const [id, text] of outbox) {
      outbox.delete(id)
      try {
        socket?.send(text)
      } catch {
        // closing; the waiter is failed by `lost`
      }
    }
    if (everReady) safe(() => opts.onReconnect?.())
    everReady = true
  }

  const authenticate = async (current: WebSocketLike): Promise<void> => {
    if (!opts.auth) return becomeReady()
    let params: unknown
    try {
      params = await opts.auth()
    } catch (error) {
      report(error)
      if (socket === current) lost(undefined)
      return
    }
    if (socket !== current) return
    request(
      'auth',
      { params },
      {
        once: true,
        onFrame: (frame) => {
          if (frame.status !== 'error') return becomeReady()
          const error = frame.error as ErrorBody | undefined
          const code = typeof error?.code === 'number' ? error.code : 500
          if (code === 401 || code === 403) {
            safe(() => opts.onAuthError?.(error as ErrorBody))
            return stop()
          }
          lost(undefined)
        },
      },
    )
  }

  function open(): void {
    clearTimeout(reconnectTimer)
    setStatus('connecting')
    ready = false
    let current: WebSocketLike
    try {
      current = factory(url)
    } catch (error) {
      report(error)
      return scheduleReconnect(undefined)
    }
    socket = current
    current.onopen = () => {
      if (socket !== current) return
      lastInbound = Date.now()
      watchdog = setInterval(checkLiveness, Math.max(1_000, liveness / 6))
      void authenticate(current)
    }
    current.onmessage = (event) => {
      if (socket !== current) return
      lastInbound = Date.now()
      const frame = readFrame(event.data)
      if (frame) handleFrame(frame)
    }
    current.onerror = () => undefined
    current.onclose = (event) => {
      if (socket === current) lost(event.code)
    }
  }

  /** A socket that went quiet (half-open TCP) is replaced. */
  function checkLiveness(): void {
    if (socket && Date.now() - lastInbound > liveness) lost(undefined)
  }

  function disconnect(next: SocketStatus): void {
    clearTimeout(reconnectTimer)
    setStatus(next)
    lost(1000)
  }

  function stop(): void {
    disconnect('closed')
    const closed = transportError('connection closed', 'connection_closed')
    for (const [id, waiter] of waiters) {
      waiters.delete(id)
      waiter.lost?.(closed)
    }
    outbox.clear()
    subscriptions.clear()
    notify()
  }

  async function onClientCall(frame: Frame): Promise<void> {
    const handler = handlers.get(frame.name ?? '')
    if (!handler) {
      write({
        kind: 'client_result',
        id: frame.id,
        error: { code: 404, message: `no handler for ${frame.name}` },
      })
      return
    }
    try {
      const data = await handler(frame.params)
      write({ kind: 'client_result', id: frame.id, data })
    } catch (error) {
      report(error)
      const message = error instanceof Error ? error.message : String(error)
      write({
        kind: 'client_result',
        id: frame.id,
        error: { code: 500, message },
      })
    }
  }

  function handleFrame(frame: Frame): void {
    switch (frame.kind) {
      case 'ping':
        return write({ kind: 'pong' })
      case 'result': {
        const id = frame.id ?? -1
        const waiter = waiters.get(id)
        const final = waiter?.once || frame.status === 'error' || !frame.more
        if (final) waiters.delete(id)
        return waiter?.onFrame(frame)
      }
      case 'end':
        waiters.delete(frame.id ?? -1)
        return
      case 'client_call':
        void onClientCall(frame)
        return
      case 'publish':
        return subscriptions.onPublish(frame)
      case 'reset':
        return subscriptions.onReset(frame)
      case 'unsubscribed':
        return subscriptions.onUnsubscribed(frame)
    }
  }

  /** Queues a request frame; sent now when authenticated, else on ready. */
  const enqueue = (id: number, text: string): void => {
    if (ready && socket) {
      try {
        socket.send(text)
      } catch {
        // closing; `lost` fails the waiter
      }
    } else outbox.set(id, text)
  }

  const pushResult = (result: ResultMap<R, ParamsMap<R>>): void => {
    if (queue.length >= MAX_STREAM_QUEUE) queue.shift()
    queue.push(result)
    notify()
  }

  async function* gen(): AsyncGenerator<ResultMap<R, ParamsMap<R>>> {
    for (;;) {
      while (queue.length) yield queue.shift()!
      if (status === 'closed') return
      await new Promise<void>((resolve) => {
        wake = resolve
      })
    }
  }

  open()

  return {
    async send(params) {
      if (status === 'closed')
        throw transportError('connection closed', 'connection_closed')
      for (const action of Object.keys(params)) {
        const id = nextId++
        const result = (frame: Frame): void =>
          pushResult({ [action]: toResult(frame) } as ResultMap<
            R,
            ParamsMap<R>
          >)
        waiters.set(id, {
          onFrame: result,
          lost: (error) =>
            result({
              kind: 'result',
              status: 'error',
              error: { code: 503, type: error.code, message: error.message },
            }),
        })
        const call = {
          kind: 'call',
          id,
          action,
          params: (params as Record<string, unknown>)[action],
        }
        enqueue(id, JSON.stringify(call))
      }
    },
    stream: gen(),
    call: (action, params) =>
      new Promise((resolve, reject) => {
        if (status === 'closed')
          return reject(
            transportError('connection closed', 'connection_closed'),
          )
        const id = nextId++
        const text = JSON.stringify({ kind: 'call', id, action, params })
        if (text.length > maxFrame)
          return reject(
            transportError('call frame too large', 'frame_too_large'),
          )
        const timer = setTimeout(() => {
          waiters.delete(id)
          outbox.delete(id)
          reject(transportError('call timed out', 'timeout'))
        }, callTimeout)
        waiters.set(id, {
          once: true,
          onFrame: (frame) => {
            clearTimeout(timer)
            resolve(toResult(frame) as never)
          },
          lost: (error) => {
            clearTimeout(timer)
            outbox.delete(id)
            reject(error)
          },
        })
        enqueue(id, text)
      }),
    subscribe: (channel, key, subscriptionHandlers) =>
      subscriptions.subscribe(channel, key, subscriptionHandlers),
    pause: () => {
      if (status !== 'closed') disconnect('paused')
    },
    resume: () => {
      if (status === 'closed') return
      if (status === 'open') {
        // Quiet for long: probably dead after a background period.
        if (Date.now() - lastInbound <= liveness / 2) return
        lost(undefined)
      } else if (socket) return // a connect is already in flight
      attempt = 0
      open()
    },
    status: () => status,
    close: stop,
  }
}
