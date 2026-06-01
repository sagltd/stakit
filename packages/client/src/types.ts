// Public types for the stakit client. The client is generic over the `Router`
// type emitted by stakit-router (`{ serverActions, clientActions }`), recovering
// per-action param/result types through indexed access. See docs/transport.md.

/** Error body carried by an action error (mirrors the server `ErrorBody`). */
export interface ErrorBody {
  readonly code: number
  readonly message: string
  readonly fields?: Record<string, readonly string[]>
}

/**
 * The outcome of one action call: success with typed `data`, or an application
 * error. Network failures never appear here — they reject / go to `onError`.
 * `isOk` / `isError` narrow this union.
 */
export type ActionResult<T> =
  | { readonly status: 'ok'; readonly data: T }
  | { readonly status: 'error'; readonly error: ErrorBody }

// ── shape extraction (infer-based, so the generated Router needs no index sig) ─

type ServerActionsOf<R> = R extends { serverActions: infer S } ? S : never
type ClientActionsOf<R> = R extends { clientActions: infer C } ? C : never

/** A partial map of `action -> params` accepted by `fetch` / `stream`. */
export type ParamsMap<R> = {
  [K in keyof ServerActionsOf<R>]?: ServerActionsOf<R>[K] extends {
    params: infer P
  }
    ? P
    : never
}

/** The `action -> ActionResult` map returned for a given `ParamsMap`. */
export type ResultMap<R, P extends ParamsMap<R>> = {
  [K in keyof P & keyof ServerActionsOf<R>]: ServerActionsOf<R>[K] extends {
    result: infer Res
  }
    ? ActionResult<Res>
    : never
}

/** Handlers for client actions the server may invoke over a duplex connection. */
export type ClientActionHandlers<R> = {
  [K in keyof ClientActionsOf<R>]: (
    params: ClientActionsOf<R>[K] extends { params: infer P } ? P : never,
  ) =>
    | (ClientActionsOf<R>[K] extends { return: infer Ret } ? Ret : never)
    | Promise<ClientActionsOf<R>[K] extends { return: infer Ret } ? Ret : never>
}

/** A file payload for multipart upload. */
export type FileInput = File | Blob | Uint8Array | ArrayBuffer

export type HeadersMap = Record<string, string>

export type HttpMethod = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'

/** Per-call overrides. Every field optional; the client base is never mutated. */
export interface CallOpts<R> {
  /** Override the base url for this call (fan out to another server). */
  readonly url?: string
  /** Headers merged over the base headers (per-call wins on key clash). */
  readonly headers?: HeadersMap
  /** Files uploaded as multipart `file` parts (HTTP only). */
  readonly files?: readonly FileInput[]
  /** Override the HTTP method. */
  readonly method?: HttpMethod
  /** Per-call client action handlers (override base for this call). */
  readonly defineClientActions?: Partial<ClientActionHandlers<R>>
}

/** Options for `createClient`. */
export interface ClientOptions<R> {
  /** Base url for HTTP (unary). */
  readonly url: string
  /** Base headers sent on every call. */
  readonly headers?: HeadersMap
  /** Url for the stream transport (defaults to `url`). */
  readonly streamUrl?: string
  /** Url for the websocket transport (defaults to `url`, `http(s)`→`ws(s)`). */
  readonly wsUrl?: string
  /** Base client action handlers (called by the server over a connection). */
  readonly defineClientActions?: Partial<ClientActionHandlers<R>>
  /** Maps a transport error before it is thrown. Only real network failures. */
  readonly onError?: (error: Error) => Error
}
