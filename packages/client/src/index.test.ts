// Runtime parity matrix — same cases as the Rust client's tests/parity.rs,
// run against a Bun.serve mock that follows docs/transport.md.

import { afterAll, beforeAll, expect, test } from 'bun:test'
import type { Server } from 'bun'

import { createClient, isError, isOk } from './index'

interface TestRouter {
  serverActions: {
    greet: {
      params: { name: string }
      result: { message: string }
      kind: 'unary'
    }
    whoami: {
      params: Record<string, never>
      result: { message: string }
      kind: 'unary'
    }
    boom: {
      params: Record<string, never>
      result: { message: string }
      kind: 'unary'
    }
    save_image: {
      params: { fileName: string }
      result: { bytes: number }
      kind: 'unary'
    }
    count: { params: { n: number }; result: number; kind: 'stream' }
    failing: { params: { n: number }; result: number; kind: 'stream' }
    progress: { params: { n: number }; result: number; kind: 'stream' }
  }
  clientActions: {
    showToast: { params: { text: string }; return: string }
  }
}

// ── mock server (implements the wire contract) ───────────────────────────────

interface WsData {
  pending: Map<number, (value: unknown) => void>
  nextCid: number
}

function runAction(
  name: string,
  params: Record<string, unknown>,
  token: string | null,
  fileBytes: number,
  serverName: string,
): unknown {
  switch (name) {
    case 'greet':
      if (typeof params.name !== 'string' || params.name.length < 1) {
        return {
          status: 'error',
          error: {
            code: 422,
            message: 'validation',
            fields: { name: ['min_len'] },
          },
        }
      }
      return {
        status: 'ok',
        data: { message: `hello ${params.name} from ${serverName}` },
      }
    case 'whoami':
      return { status: 'ok', data: { message: token ?? '' } }
    case 'boom':
      return { status: 'error', error: { code: 500, message: 'boom: nope' } }
    case 'save_image':
      return { status: 'ok', data: { bytes: fileBytes } }
    default:
      return {
        status: 'error',
        error: { code: 404, message: `unknown ${name}` },
      }
  }
}

function makeServer(serverName: string): Server<WsData> {
  return Bun.serve<WsData>({
    port: 0,
    async fetch(req, server) {
      const url = new URL(req.url)
      const token = req.headers.get('authorization')

      if (url.pathname === '/ws') {
        if (server.upgrade(req, { data: { pending: new Map(), nextCid: 1 } }))
          return undefined
        return new Response('upgrade failed', { status: 400 })
      }

      const calls = JSON.parse(url.searchParams.get('q') ?? '{}') as Record<
        string,
        Record<string, unknown>
      >

      if (url.pathname === '/stream') {
        const [name, params] = Object.entries(calls)[0] ?? ['', {}]
        const encoder = new TextEncoder()
        const body = new ReadableStream({
          start(controller) {
            const line = (obj: unknown) =>
              controller.enqueue(encoder.encode(`${JSON.stringify(obj)}\n`))
            if (name === 'count') {
              for (let i = 0; i < Number(params.n); i++)
                line({ type: 'next', data: i })
              line({ type: 'end' })
            } else if (name === 'failing') {
              line({ type: 'next', data: 1 })
              line({
                type: 'error',
                error: { code: 500, message: 'mid-stream boom' },
              })
            }
            controller.close()
          },
        })
        return new Response(body, {
          headers: { 'content-type': 'application/x-ndjson' },
        })
      }

      // /rpc — collect any uploaded files
      let fileBytes = 0
      if (req.method !== 'GET') {
        const contentType = req.headers.get('content-type') ?? ''
        if (contentType.includes('multipart/form-data')) {
          const form = await req.formData()
          for (const value of form.getAll('file')) {
            if (value instanceof Blob) fileBytes += value.size
          }
        }
      }

      const result: Record<string, unknown> = {}
      for (const [name, params] of Object.entries(calls)) {
        result[name] = runAction(name, params, token, fileBytes, serverName)
      }
      return Response.json(result)
    },
    websocket: {
      open() {},
      async message(ws, raw) {
        const frame = JSON.parse(String(raw)) as {
          kind: string
          id: number
          action?: string
          params?: Record<string, unknown>
          data?: unknown
        }
        const state = ws.data
        if (frame.kind === 'client_result') {
          const resolve = state.pending.get(frame.id)
          if (resolve) {
            state.pending.delete(frame.id)
            resolve(frame.data)
          }
          return
        }
        if (frame.kind !== 'call') return
        if (frame.action === 'greet') {
          ws.send(
            JSON.stringify({
              kind: 'result',
              id: frame.id,
              status: 'ok',
              data: {
                message: `hello ${frame.params?.name} from ${serverName}`,
              },
            }),
          )
        } else if (frame.action === 'progress') {
          const n = Number(frame.params?.n ?? 0)
          for (let i = 0; i < n; i++) {
            const cid = state.nextCid++
            const ack = new Promise((resolve) =>
              state.pending.set(cid, resolve),
            )
            ws.send(
              JSON.stringify({
                kind: 'client_call',
                id: cid,
                name: 'showToast',
                params: { text: `step ${i}` },
              }),
            )
            await ack
            ws.send(
              JSON.stringify({
                kind: 'result',
                id: frame.id,
                status: 'ok',
                data: i,
              }),
            )
          }
          ws.send(JSON.stringify({ kind: 'end', id: frame.id }))
        }
      },
    },
  })
}

let serverA: Server<WsData>
let serverB: Server<WsData>

beforeAll(() => {
  serverA = makeServer('A')
  serverB = makeServer('B')
})
afterAll(() => {
  serverA.stop(true)
  serverB.stop(true)
})

function clientFor(server: Server<WsData>) {
  const origin = `http://localhost:${server.port}`
  return createClient<TestRouter>({
    url: `${origin}/rpc`,
    headers: { authorization: 'root' },
    streamUrl: `${origin}/stream`,
    wsUrl: `${origin}/ws`,
    defineClientActions: { showToast: () => 'ok' },
  })
}

// ── matrix ───────────────────────────────────────────────────────────────────

test('1: unary ok returns typed data', async () => {
  const r = await clientFor(serverA).fetch({ greet: { name: 'sam' } })
  expect(isOk(r.greet)).toBe(true)
  if (isOk(r.greet)) expect(r.greet.data.message).toBe('hello sam from A')
})

test('2: unary app error is an action error', async () => {
  const r = await clientFor(serverA).fetch({ boom: {} })
  expect(isError(r.boom)).toBe(true)
  if (isError(r.boom)) {
    expect(r.boom.error.code).toBe(500)
    expect(r.boom.error.message).toContain('boom')
  }
})

test('3: validation error has fields', async () => {
  const r = await clientFor(serverA).fetch({ greet: { name: '' } })
  if (isError(r.greet)) {
    expect(r.greet.error.code).toBe(422)
    expect(r.greet.error.fields?.name).toBeDefined()
  } else {
    throw new Error('expected error')
  }
})

test('4: mixed ok + error narrow independently', async () => {
  const r = await clientFor(serverA).fetch({ greet: { name: 'a' }, boom: {} })
  expect(isOk(r.greet)).toBe(true)
  expect(isError(r.boom)).toBe(true)
})

test('5: stream yields items then ends', async () => {
  const values: number[] = []
  for await (const frame of clientFor(serverA).stream({ count: { n: 4 } })) {
    if (isOk(frame.count)) values.push(frame.count.data)
  }
  expect(values).toEqual([0, 1, 2, 3])
})

test('6: stream error terminates', async () => {
  const frames = []
  for await (const frame of clientFor(serverA).stream({ failing: { n: 9 } })) {
    frames.push(frame.failing)
  }
  expect(frames.length).toBe(2)
  expect(isOk(frames[0]!)).toBe(true)
  expect(isError(frames[1]!)).toBe(true)
})

test('7: websocket roundtrip', async () => {
  const conn = clientFor(serverA).connect()
  await conn.send({ greet: { name: 'ws' } })
  const first = await conn.stream.next()
  const value = first.value as {
    greet: { status: string; data: { message: string } }
  }
  expect(value.greet.status).toBe('ok')
  expect(value.greet.data.message).toBe('hello ws from A')
  conn.close()
})

test('8: websocket server-to-client call', async () => {
  let toasts = 0
  const conn = clientFor(serverA).connect({
    defineClientActions: {
      showToast: () => {
        toasts++
        return 'ok'
      },
    },
  })
  await conn.send({ progress: { n: 2 } })
  let results = 0
  for await (const frame of conn.stream) {
    if ('progress' in frame) results++
    if (results >= 2) break
  }
  conn.close()
  expect(toasts).toBe(2)
  expect(results).toBe(2)
})

test('9: per-call url override leaves base untouched', async () => {
  const client = clientFor(serverA)
  const originB = `http://localhost:${serverB.port}`

  const base = await client.fetch({ greet: { name: 'x' } })
  if (isOk(base.greet)) expect(base.greet.data.message).toBe('hello x from A')

  const overridden = await client.fetch(
    { greet: { name: 'x' } },
    { url: `${originB}/rpc` },
  )
  if (isOk(overridden.greet))
    expect(overridden.greet.data.message).toBe('hello x from B')

  const again = await client.fetch({ greet: { name: 'x' } })
  if (isOk(again.greet)) expect(again.greet.data.message).toBe('hello x from A')
})

test('10: per-call headers merge and leave base untouched', async () => {
  const client = clientFor(serverA)
  const base = await client.fetch({ whoami: {} })
  if (isOk(base.whoami)) expect(base.whoami.data.message).toBe('root')

  const scoped = await client.fetch(
    { whoami: {} },
    { headers: { authorization: 'scoped' } },
  )
  if (isOk(scoped.whoami)) expect(scoped.whoami.data.message).toBe('scoped')

  const again = await client.fetch({ whoami: {} })
  if (isOk(again.whoami)) expect(again.whoami.data.message).toBe('root')
})

test('11: files upload via multipart', async () => {
  const r = await clientFor(serverA).fetch(
    { save_image: { fileName: 'a.png' } },
    { files: [new Uint8Array([1, 2, 3, 4, 5]), new Uint8Array([6, 7, 8])] },
  )
  if (isOk(r.save_image)) expect(r.save_image.data.bytes).toBe(8)
})

test('12: setHeaders replace and functional update', async () => {
  const client = clientFor(serverA)

  client.setHeaders({ authorization: 'replaced' })
  const replaced = await client.fetch({ whoami: {} })
  if (isOk(replaced.whoami))
    expect(replaced.whoami.data.message).toBe('replaced')

  client.setHeaders((prev) => ({ ...prev, authorization: 'updated' }))
  const updated = await client.fetch({ whoami: {} })
  if (isOk(updated.whoami)) expect(updated.whoami.data.message).toBe('updated')
})

test('13: transport failure rejects', async () => {
  const client = createClient<TestRouter>({ url: 'http://127.0.0.1:1/rpc' })
  await expect(client.fetch({ greet: { name: 'x' } })).rejects.toThrow()
})
