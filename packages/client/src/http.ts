// HTTP wire helpers shared by the unary and stream transports: request
// building (params as a JSON body or a multipart `q` field — only an explicit
// GET puts them in the url) and JSONL frame parsing. See docs/transport.md.

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

/** Parses one JSONL line; blank or malformed lines yield `null` (skipped). */
export function parseFrame(line: string): WireFrame | null {
  if (!line.trim()) return null
  try {
    const frame: unknown = JSON.parse(line)
    if (typeof frame !== 'object' || frame === null) return null
    if (typeof (frame as { type?: unknown }).type !== 'string') return null
    return frame as WireFrame
  } catch {
    return null
  }
}

function toBlob(file: FileInput): Blob {
  if (file instanceof Blob) return file
  return new Blob([file as BlobPart])
}
