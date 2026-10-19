// HTTP wire helpers shared by the unary and stream transports: request
// building (params as a JSON body or a multipart `q` field — only an explicit
// GET puts them in the url) and JSONL frame parsing. See docs/transport.md.

import { TransportError } from './transport-error'

import type {
  CallOpts,
  FetchInit,
  FileInput,
  HeadersMap,
  HttpMethod,
} from './types'

export interface HttpRequest {
  readonly url: string
  readonly init: FetchInit
}

/** `signal` as an init field, only when there is one. */
function signalField(signal?: AbortSignal): { signal?: AbortSignal } {
  return signal ? { signal } : {}
}

export interface WireFrame {
  readonly type: string
  readonly data?: unknown
  readonly error?: unknown
}

/**
 * The unary request (docs/transport.md): multipart when files are attached
 * (params in the `q` form field), otherwise the JSON payload request.
 */
export function unaryRequest<R>(
  url: string,
  params: unknown,
  headers: HeadersMap,
  opts?: CallOpts<R>,
): HttpRequest {
  const method = opts?.method ?? 'POST'
  const files = opts?.files ?? []
  if (files.length === 0)
    return payloadRequest(url, params, method, headers, opts?.signal)
  if (method === 'GET') throw new Error('files cannot be sent with GET')

  const form = new FormData()
  form.append('q', JSON.stringify(params))
  for (const file of files) form.append('file', toBlob(file))
  // fetch sets the multipart content-type (with its boundary) itself
  return {
    url,
    init: {
      method,
      headers: withoutContentType(headers),
      body: form,
      ...signalField(opts?.signal),
    },
  }
}

/**
 * Params as a JSON body — never in the url, so they cannot leak into access
 * logs. Only an explicit GET carries them in `?q=`.
 */
export function payloadRequest(
  url: string,
  params: unknown,
  method: HttpMethod,
  headers: HeadersMap,
  signal?: AbortSignal,
): HttpRequest {
  if (method === 'GET') {
    const target = new URL(url)
    target.searchParams.set('q', JSON.stringify(params))
    return {
      url: target.toString(),
      init: { method, headers, ...signalField(signal) },
    }
  }
  return {
    url,
    init: {
      method,
      headers: {
        ...withoutContentType(headers),
        'content-type': 'application/json',
      },
      body: JSON.stringify(params),
      ...signalField(signal),
    },
  }
}

function withoutContentType(headers: HeadersMap): HeadersMap {
  const result: HeadersMap = {}
  for (const [name, value] of Object.entries(headers)) {
    if (name.toLowerCase() !== 'content-type') result[name] = value
  }
  return result
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function isErrorBody(value: unknown): boolean {
  if (
    !isRecord(value) ||
    !Number.isInteger(value.code) ||
    typeof value.message !== 'string'
  )
    return false
  if (value.type !== undefined && typeof value.type !== 'string') return false
  return (
    value.fields === undefined ||
    (isRecord(value.fields) &&
      Object.values(value.fields).every(
        (messages) =>
          Array.isArray(messages) &&
          messages.every((message: unknown) => typeof message === 'string'),
      ))
  )
}

function isWireFrame(value: unknown): value is WireFrame {
  if (
    !isRecord(value) ||
    typeof value.type !== 'string' ||
    value.type.length === 0
  )
    return false
  if (value.type === 'next')
    return Object.hasOwn(value, 'data') && !Object.hasOwn(value, 'error')
  if (value.type === 'error')
    return isErrorBody(value.error) && !Object.hasOwn(value, 'data')
  if (value.type === 'end')
    return !Object.hasOwn(value, 'data') && !Object.hasOwn(value, 'error')
  // Heartbeats and future typed frames are intentionally ignored by the caller.
  return true
}

/** Blank lines are skipped; malformed protocol frames reject without exposing their payload. */
export function parseFrame(line: string, status = 0): WireFrame | null {
  if (!line.trim()) return null
  let frame: unknown
  try {
    frame = JSON.parse(line)
  } catch {
    throw new TransportError('invalid stream frame', {
      status,
      code: 'stream_invalid_frame',
    })
  }
  if (!isWireFrame(frame))
    throw new TransportError('invalid stream frame', {
      status,
      code: 'stream_invalid_frame',
    })
  return frame
}

function toBlob(file: FileInput): Blob {
  if (file instanceof Blob) return file
  return new Blob([file as BlobPart])
}
