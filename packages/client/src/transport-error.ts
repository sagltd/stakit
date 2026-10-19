// A transport-level failure, with what the response said about it, so callers
// can show "file too large" or back off on a rate limit without parsing text.

import type { FetchResponse } from './types'

/** How much of a failed response's body is kept. */
const MAX_BODY_CHARS = 1_000

/** What a {@link TransportError} knows about the failure. */
export interface TransportErrorDetails {
  /** HTTP status of the answer (0 when none arrived). */
  readonly status: number
  /** Machine-readable code: `x-error-code`, `stream_interrupted`, or `stream_invalid_frame`. */
  readonly code?: string
  /** Seconds to wait before retrying (`Retry-After`). */
  readonly retryAfter?: number
  /** The start of the response body. */
  readonly body?: string
}

/**
 * The request failed at the transport level: a non-2xx answer, or a stream
 * that broke off before it finished. Action errors never take this path.
 */
export class TransportError extends Error {
  readonly status: number
  readonly code: string | undefined
  readonly retryAfter: number | undefined
  readonly body: string | undefined

  constructor(message: string, details: TransportErrorDetails) {
    super(message)
    this.name = 'TransportError'
    this.status = details.status
    this.code = details.code
    this.retryAfter = details.retryAfter
    this.body = details.body
  }
}

/** Delta-seconds from a `Retry-After` header; HTTP dates are not parsed. */
function retryAfterSeconds(value: string | null): number | undefined {
  if (value === null || value.trim() === '') return undefined
  const seconds = Number(value)
  return Number.isFinite(seconds) && seconds >= 0 ? seconds : undefined
}

/** The error for a non-2xx `response`, reading (and so releasing) its body. */
export async function failedResponse(
  response: FetchResponse,
): Promise<TransportError> {
  const body = await response.text().then(
    (text) => text.slice(0, MAX_BODY_CHARS),
    () => undefined,
  )
  return new TransportError(`request failed with status ${response.status}`, {
    status: response.status,
    code: response.headers.get('x-error-code') ?? undefined,
    retryAfter: retryAfterSeconds(response.headers.get('retry-after')),
    body,
  })
}
