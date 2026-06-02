// Automated cross-language integration test: spawns the REAL (refactored) Rust
// server and drives it with the TypeScript client over every transport,
// including server→client `client_call`. Run from this dir:
//
//   cargo build && bun test integration.test.ts

import { afterAll, beforeAll, expect, test } from 'bun:test'
import type { Subprocess } from 'bun'

import { createClient, isError, isOk } from '../../packages/client/src/index'
import type { Router } from './types'

const base = 'http://127.0.0.1:3007'
let server: Subprocess

beforeAll(async () => {
  server = Bun.spawn(['./target/debug/axum-server-example'], {
    cwd: import.meta.dir,
    stdout: 'ignore',
    stderr: 'ignore',
  })
  // wait until it binds
  for (let i = 0; i < 200; i++) {
    try {
      await fetch(`${base}/app?q=%7B%7D`)
      return
    } catch {
      await Bun.sleep(50)
    }
  }
  throw new Error('server did not start')
})

afterAll(() => {
  server?.kill()
})

function client() {
  return createClient<Router>({
    url: `${base}/app`,
    headers: { 'x-admin': 'true' },
    streamUrl: `${base}/stream`,
    wsUrl: `${base}/ws`,
    defineClientActions: { showToast: () => 'ok' },
  })
}

test('http unary', async () => {
  const r = await client().fetch({ greet: { name: 'sam' } })
  expect(isOk(r.greet)).toBe(true)
  if (isOk(r.greet)) expect(r.greet.data.message).toBe('Hello, sam! (admin=true)')
})

test('http validation error', async () => {
  const r = await client().fetch({ greet: { name: '' } })
  expect(isError(r.greet)).toBe(true)
  if (isError(r.greet)) expect(r.greet.error.code).toBe(422)
})

test('http many actions in one request', async () => {
  const r = await client().fetch({ greet: { name: 'alice', userId: 1 }, version: null })
  if (isOk(r.greet)) expect(r.greet.data.message).toBe('Hello, alice! (admin=true)')
  if (isOk(r.version)) expect(r.version.data).toBe('stakit-example/0.1.0')
})

test('http multipart file upload', async () => {
  const r = await client().fetch(
    { save_image: { fileName: 'i.png' } },
    { files: [new Uint8Array([1, 2, 3, 4, 5])] },
  )
  if (isOk(r.save_image)) expect(r.save_image.data.bytes).toBe(5)
})

test('http stream', async () => {
  const items: number[] = []
  for await (const frame of client().stream({ count: { n: 4 } })) {
    if (isOk(frame.count)) items.push(frame.count.data)
  }
  expect(items).toEqual([0, 1, 2, 3])
})

test('websocket server→client client_call', async () => {
  let toasts = 0
  const conn = client().connect({
    defineClientActions: {
      showToast: () => {
        toasts++
        return 'ok'
      },
    },
  })
  await conn.send({ progress: { n: 3 } })
  let results = 0
  for await (const frame of conn.stream) {
    if ('progress' in frame) results++
    if (results >= 3) break
  }
  conn.close()
  expect(toasts).toBe(3)
  expect(results).toBe(3)
})
