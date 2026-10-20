// @stakit/client — typed client for stakit routers (HTTP, stream, websocket).
//
// Generic over the `Router` type emitted by stakit-router. Never throws on an
// application error — those ride in `ActionResult` and are narrowed with
// `isOk` / `isError`; only real network failures reject (or hit `onError`).

export { createClient } from './client'
export type { Client } from './client'
export type {
  Connection,
  ConnectOpts,
  GapReason,
  PublishMessage,
  SocketStatus,
  Subscription,
  WebSocketFactory,
  WebSocketLike,
} from './socket'
export { isError, isOk } from './guards'
export { TransportError } from './transport-error'
export type { TransportErrorDetails } from './transport-error'
export type {
  ActionResult,
  CallOpts,
  ChannelsOf,
  ClientActionHandlers,
  ClientOptions,
  ErrorBody,
  FetchFn,
  FetchInit,
  FetchResponse,
  FileInput,
  HeadersMap,
  HttpMethod,
  ParamsMap,
  PublishHandlers,
  ResultMap,
} from './types'
