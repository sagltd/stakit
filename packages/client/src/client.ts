// The stakit client: one handle, three transports (HTTP unary, HTTP stream,
// websocket duplex), generic over the generated `Router` type. Mirrors the
// Rust client and the wire contract in docs/transport.md.

import { parseFrame, payloadRequest, unaryRequest } from './http'
import type { HttpRequest } from './http'
import { connectSocket } from './socket'
import type { ConnectOpts, Connection } from './socket'
import { TransportError, failedResponse } from './transport-error'
import type {
  CallOpts,
  ClientActionHandlers,
  ClientOptions,
  FetchFn,
  FetchResponse,
  HeadersMap,
  ParamsMap,
  ResultMap,
} from './types'

/** A stakit client handle. */
export interface Client<R> {
  /** Replaces the base headers wholesale, or via a `prev => next` updater. */
  readonly setHeaders: (
    headers: HeadersMap | ((prev: HeadersMap) => HeadersMap),
  ) => void
  /** Calls a batch of unary actions; resolves to an `action -> result` map. */
  readonly fetch: <P extends ParamsMap<R>>(
    params: P,
    opts?: CallOpts<R>,
  ) => Promise<ResultMap<R, P>>
  /** Streams a single action's frames as result maps. */
  readonly stream: <P extends ParamsMap<R>>(
    params: P,
    opts?: CallOpts<R>,
  ) => AsyncGenerator<ResultMap<R, P>>
  /**
   * Opens a reconnecting duplex websocket connection (calls, client actions,
   * pub/sub subscriptions). See `ConnectOpts` for auth, backoff and pausing.
   */
  readonly connect: (opts?: ConnectOpts<R>) => Connection<R>
  /** Per-action proxy: `client.actions.greet(params, opts?)`. */
  readonly actions: ActionsProxy<R>
}

type ActionsProxy<R> = {
  readonly [K in keyof (R extends { serverActions: infer S } ? S : never)]: (
    params: (R extends { serverActions: infer S } ? S : never)[K] extends {
      params: infer P
    }
      ? P
      : never,
    opts?: CallOpts<R>,
  ) => Promise<ResultMap<R, ParamsMap<R>>[keyof ResultMap<R, ParamsMap<R>>]>
}

interface State<R> {
  url: string
  streamUrl?: string
  wsUrl?: string
  headers: HeadersMap
  defineClientActions: Partial<ClientActionHandlers<R>>
  onError: (error: Error) => Error
  fetch: FetchFn
}

const identity = (error: Error): Error => error

// Resolved per call, so a fetch installed after `createClient` still applies.
const globalFetch: FetchFn = (input, init) => globalThis.fetch(input, init)

/**
 * `injected` called as a plain function: browser and Worker `fetch` throw
 * "Illegal invocation" when called as a method of anything but `globalThis`.
 */
function plainCall(injected: FetchFn | undefined): FetchFn {
  return injected ? (input, init) => injected(input, init) : globalFetch
}

/** Creates a stakit client generic over the generated `Router` type. */
export function createClient<R>(options: ClientOptions<R>): Client<R> {
  const state: State<R> = {
    url: options.url,
    streamUrl: options.streamUrl,
    wsUrl: options.wsUrl,
    headers: { ...(options.headers ?? {}) },
    defineClientActions: options.defineClientActions ?? {},
    onError: options.onError ?? identity,
    fetch: plainCall(options.fetch),
  }

  const setHeaders: Client<R>['setHeaders'] = (headers) => {
    state.headers =
      typeof headers === 'function'
        ? { ...headers({ ...state.headers }) }
        : { ...headers }
  }

  /**
   * Sends one HTTP request; a network failure, or a non-2xx answer (as a
   * `TransportError` with its status, code and Retry-After), goes to onError.
   */
  const send = async (request: HttpRequest): Promise<FetchResponse> => {
    let response: FetchResponse
    try {
      response = await state.fetch(request.url, request.init)
    } catch (error) {
      throw state.onError(asError(error))
    }
    if (!response.ok) throw state.onError(await failedResponse(response))
    return response
  }

  const doFetch = async <P extends ParamsMap<R>>(
    params: P,
    opts?: CallOpts<R>,
  ): Promise<ResultMap<R, P>> => {
    const headers = mergeHeaders(state.headers, opts?.headers)
    let request: HttpRequest
    try {
      request = unaryRequest(opts?.url ?? state.url, params, headers, opts)
    } catch (error) {
      throw state.onError(asError(error))
    }
    const response = await send(request)
    try {
      return (await response.json()) as ResultMap<R, P>
    } catch (error) {
      throw state.onError(asError(error))
    }
  }

  async function* doStream<P extends ParamsMap<R>>(
    params: P,
    opts?: CallOpts<R>,
  ): AsyncGenerator<ResultMap<R, P>> {
    const url = opts?.url ?? state.streamUrl ?? state.url
    const headers = mergeHeaders(state.headers, opts?.headers)
    const actionKey = Object.keys(params)[0] ?? ''

    let request: HttpRequest
    try {
      request = payloadRequest(
        url,
        params,
        opts?.method ?? 'POST',
        headers,
        opts?.signal,
      )
    } catch (error) {
      throw state.onError(asError(error))
    }
    const response = await send(request)
    if (!response.body)
      throw state.onError(
        new TransportError('stream response has no body', {
          status: response.status,
        }),
      )

    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let searchOffset = 0
    try {
      for (;;) {
        let read: Awaited<ReturnType<typeof reader.read>>
        try {
          read = await reader.read()
        } catch (error) {
          throw state.onError(asError(error))
        }
        // At the end, flush the decoder and treat what is left as a last
        // line (JSON Lines allows one without its newline).
        buffer += read.done
          ? `${decoder.decode()}\n`
          : decoder.decode(read.value, { stream: true })
        let newline = buffer.indexOf('\n', searchOffset)
        while (newline >= 0) {
          let frame: ReturnType<typeof parseFrame>
          try {
            frame = parseFrame(buffer.slice(0, newline), response.status)
          } catch (error) {
            throw state.onError(asError(error))
          }
          buffer = buffer.slice(newline + 1)
          newline = buffer.indexOf('\n')
          if (frame?.type === 'next') {
            yield {
              [actionKey]: { status: 'ok', data: frame.data },
            } as ResultMap<R, P>
          } else if (frame?.type === 'error') {
            yield {
              [actionKey]: { status: 'error', error: frame.error },
            } as ResultMap<R, P>
            return
          } else if (frame?.type === 'end') {
            return
          }
          // Blank lines and unknown types (e.g. `heartbeat`) are skipped.
        }
        // This partial line has no newline. Only search newly decoded text
        // after the next read, rather than rescanning its whole prefix.
        searchOffset = buffer.length
        if (read.done)
          throw state.onError(
            new TransportError('the stream ended before it finished', {
              status: response.status,
              code: 'stream_interrupted',
            }),
          )
      }
    } finally {
      // Release the connection when the consumer stops early or an error ends
      // the stream (a no-op once the body was read to its end).
      await reader.cancel().catch(() => undefined)
    }
  }

  const connect: Client<R>['connect'] = (opts = {}) =>
    connectSocket({
      url: toWsUrl(opts.url ?? state.wsUrl ?? state.url),
      handlers: handlerMap(state.defineClientActions, opts.defineClientActions),
      publishHandlers: handlerMap(opts.publishHandlers),
      opts,
    })

  const actions = new Proxy({} as ActionsProxy<R>, {
    get(_target, name: string) {
      return (params: unknown, opts?: CallOpts<R>) =>
        doFetch({ [name]: params } as ParamsMap<R>, opts).then(
          (result) => (result as Record<string, unknown>)[name],
        )
    },
  })

  return { setHeaders, fetch: doFetch, stream: doStream, connect, actions }
}

// ── helpers ──────────────────────────────────────────────────────────────────

function mergeHeaders(base: HeadersMap, extra?: HeadersMap): HeadersMap {
  return { ...base, ...(extra ?? {}) }
}

/**
 * Own, function-valued handlers only (no prototype names like `toString`; an
 * explicit `undefined` doesn't erase a base handler).
 */
function handlerMap(
  ...sources: (object | undefined)[]
): Map<string, (params: unknown) => unknown> {
  const map = new Map<string, (params: unknown) => unknown>()
  for (const source of sources)
    for (const [name, handler] of Object.entries(source ?? {}))
      if (typeof handler === 'function') map.set(name, handler)
  return map
}

function toWsUrl(url: string): string {
  if (url.startsWith('https://')) return `wss://${url.slice('https://'.length)}`
  if (url.startsWith('http://')) return `ws://${url.slice('http://'.length)}`
  return url
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value))
}
