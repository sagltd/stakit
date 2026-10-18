// Transport edge cases: structured HTTP failures, abandoned and aborted
// streams, truncated bodies, and how an injected fetch is called.

import { afterAll, beforeAll, expect, test } from 'bun:test'
import type { Server } from 'bun'

import { TransportError, createClient, isOk } from './index'
import type { FetchFn } from './index'

interface TestRouter {
  serverActions: {
    greet: {
      params: { name: string }
      result: { message: string }
      kind: 'unary'
    }
    count: { params: { n: number }; result: number; kind: 'stream' }
  }
  clientActions: Record<string, never>
}

const encoder = new TextEncoder()
/** Whether the endless stream saw its reader cancelled. */
let endlessCancelled = false

/** One JSONL body per path; `null` streams heartbeats until cancelled. */
const bodies: Record<string, string | null> = {
  '/truncated': '{"type":"next","data":1}\n',
  '/unterminated': '{"type":"next","data":3}\n{"type":"end"}',
  '/endless': null,
}

let server: Server<undefined>

beforeAll(() => {
  server = Bun.serve({
    port: 0,
    fetch(req) {
      const path = new URL(req.url).pathname
      if (path === '/limited') {
        return new Response('slow down', {
          status: 429,
          headers: { 'x-error-code': 'RATE_LIMITED', 'retry-after': '7' },
        })
      }
      if (path === '/rpc') {
        return Response.json({
          greet: { status: 'ok', data: { message: 'hi' } },
        })
      }
      const body = bodies[path]
      if (body === undefined) return new Response('missing', { status: 404 })
      if (body !== null) return new Response(body)
      let timer: ReturnType<typeof setInterval> | undefined
      const stream = new ReadableStream({
        start(controller) {
          controller.enqueue(encoder.encode('{"type":"next","data":0}\n'))
          timer = setInterval(() => {
            controller.enqueue(encoder.encode('{"type":"heartbeat"}\n'))
          }, 20)
        },
        cancel() {
          endlessCancelled = true
          clearInterval(timer)
        },
      })
      return new Response(stream)
    },
  })
})

afterAll(() => {
  server.stop(true)
})

function clientAt(path: string, onError?: (error: Error) => Error) {
  const origin = `http://localhost:${server.port}`
  return createClient<TestRouter>({
    url: `${origin}${path}`,
    streamUrl: `${origin}${path}`,
    onError,
  })
}

async function drain(stream: AsyncGenerator<unknown>): Promise<unknown[]> {
  const frames: unknown[] = []
  for await (const frame of stream) frames.push(frame)
  return frames
}

test('a non-2xx answer rejects with its status, error code and Retry-After', async () => {
  const seen: Error[] = []
  const client = clientAt('/limited', (error) => {
    seen.push(error)
    return error
  })

  const failure = await client.fetch({ greet: { name: 'x' } }).catch((e) => e)

  expect(failure).toBeInstanceOf(TransportError)
  expect(failure.status).toBe(429)
  expect(failure.code).toBe('RATE_LIMITED')
  expect(failure.retryAfter).toBe(7)
  expect(failure.body).toBe('slow down')
  expect(seen).toEqual([failure])
})

test('an injected fetch is called as a plain function, not a method', async () => {
  const receivers: unknown[] = []
  const injected: FetchFn = function (this: unknown, input, init) {
    receivers.push(this)
    return fetch(input, init)
  }
  const origin = `http://localhost:${server.port}`
  const client = createClient<TestRouter>({
    url: `${origin}/rpc`,
    fetch: injected,
  })

  const r = await client.fetch({ greet: { name: 'x' } })

  expect(isOk(r.greet)).toBe(true)
  expect(receivers).toHaveLength(1)
  expect(receivers[0] === undefined || receivers[0] === globalThis).toBe(true)
})

test('leaving a stream early cancels its body', async () => {
  endlessCancelled = false
  for await (const frame of clientAt('/endless').stream({ count: { n: 1 } })) {
    void frame
    break
  }
  await Bun.sleep(200)
  expect(endlessCancelled).toBe(true)
})

test('an abort signal stops a stream', async () => {
  const controller = new AbortController()
  const frames = clientAt('/endless').stream(
    { count: { n: 1 } },
    { signal: controller.signal },
  )
  const first = await frames.next()
  expect(first.done).toBe(false)
  controller.abort()
  await expect(drain(frames)).rejects.toThrow()
})

test('a body that ends before its end frame rejects through onError', async () => {
  const seen: Error[] = []
  const client = clientAt('/truncated', (error) => {
    seen.push(error)
    return error
  })

  const failure = await drain(client.stream({ count: { n: 1 } })).catch(
    (e) => e,
  )

  expect(failure).toBeInstanceOf(TransportError)
  expect(failure.code).toBe('stream_interrupted')
  expect(seen).toEqual([failure])
})

test('a last line without its newline still counts', async () => {
  const frames = await drain(
    clientAt('/unterminated').stream({ count: { n: 1 } }),
  )
  expect(frames).toEqual([{ count: { status: 'ok', data: 3 } }])
})

test('a stream request that cannot be built goes through onError', async () => {
  const client = createClient<TestRouter>({
    url: 'not a url',
    onError: (error) => new Error(`mapped: ${error.message}`),
  })
  const frames = client.stream({ count: { n: 1 } }, { method: 'GET' })
  await expect(drain(frames)).rejects.toThrow('mapped:')
})
