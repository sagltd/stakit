// @stakit/client — typed client for stakit routers (HTTP, stream, websocket).
//
// Generic over the `Router` type emitted by stakit-router. Never throws on an
// application error — those ride in `ActionResult` and are narrowed with
// `isOk` / `isError`; only real network failures reject (or hit `onError`).

export { createClient } from './client'
export type { Client, Connection } from './client'
export { isError, isOk } from './guards'
export type {
  ActionResult,
  CallOpts,
  ClientActionHandlers,
  ClientOptions,
  ErrorBody,
  FileInput,
  HeadersMap,
  HttpMethod,
  ParamsMap,
  ResultMap,
} from './types'
