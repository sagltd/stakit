// Server→client duplex frames, validated before use (a malformed frame from a
// buggy or hostile server is dropped, never trusted). Wire: docs/transport.md.

import type { ErrorBody } from './types'

/** A validated server frame. */
export interface Frame {
  readonly kind: string
  readonly id?: number
  readonly name?: string
  readonly params?: unknown
  readonly status?: string
  readonly data?: unknown
  readonly error?: unknown
  readonly topic?: string
  readonly seq?: number
  readonly epoch?: number
  readonly more?: boolean
  readonly reason?: string
}

/** A subscribe acknowledgement. */
export interface Ack {
  readonly topic: string
  readonly epoch: number
  readonly seq: number
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)

const isCount = (value: unknown): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0

const optional = (
  frame: Record<string, unknown>,
  field: string,
  check: (value: unknown) => boolean,
): boolean => frame[field] === undefined || check(frame[field])

const isString = (value: unknown): boolean => typeof value === 'string'

/** Parses one text frame; `null` for anything that isn't a valid frame. */
export function readFrame(raw: unknown): Frame | null {
  if (typeof raw !== 'string') return null
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    return null
  }
  if (!isRecord(value) || typeof value.kind !== 'string') return null
  const valid =
    optional(value, 'id', isCount) &&
    optional(value, 'seq', isCount) &&
    optional(value, 'epoch', isCount) &&
    optional(value, 'name', isString) &&
    optional(value, 'topic', isString) &&
    optional(value, 'status', isString) &&
    optional(value, 'reason', isString)
  return valid ? (value as unknown as Frame) : null
}

/** A subscribe result's data, if well-formed. */
export function readAck(data: unknown): Ack | null {
  if (!isRecord(data)) return null
  const { topic, epoch, seq } = data
  if (typeof topic !== 'string' || !isCount(epoch) || !isCount(seq)) return null
  return { topic, epoch, seq }
}

/** An error body, or a generic one when the server sent something else. */
export function readError(value: unknown): ErrorBody {
  if (
    isRecord(value) &&
    typeof value.code === 'number' &&
    typeof value.message === 'string'
  )
    return value as unknown as ErrorBody
  return { code: 500, message: 'malformed error from server' }
}

/** Order-independent JSON for a subscription key (same key → same string). */
export function stableKey(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableKey).join(',')}]`
  if (isRecord(value))
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stableKey(value[key])}`)
      .join(',')}}`
  return JSON.stringify(value) ?? 'null'
}
