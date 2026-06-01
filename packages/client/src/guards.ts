// Type-guard helpers that narrow an ActionResult to its ok / error branch.

import type { ActionResult, ErrorBody } from './types'

/**
 * Narrows an `ActionResult<T>` to its success branch, exposing `.data`.
 *
 * ```ts
 * if (isOk(r.greet)) r.greet.data // typed
 * ```
 */
export function isOk<T>(
  result: ActionResult<T>,
): result is { readonly status: 'ok'; readonly data: T } {
  return result.status === 'ok'
}

/**
 * Narrows an `ActionResult<T>` to its error branch, exposing `.error`.
 *
 * ```ts
 * if (isError(r.greet)) r.greet.error.code
 * ```
 */
export function isError<T>(
  result: ActionResult<T>,
): result is { readonly status: 'error'; readonly error: ErrorBody } {
  return result.status === 'error'
}
