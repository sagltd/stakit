// Live demo of the TypeScript client against the running example server.
//
// Start the server first (`cargo run --bin axum-server-example`), then:
//   bun run example-client.ts
//
// Types come from the generated `types.d.ts` (written by the server on startup).

import { createClient, isError, isOk } from '../../packages/client/src/index'
import type { Router } from './types'

const base = 'http://127.0.0.1:3007'

const client = createClient<Router>({
  url: `${base}/app`,
  headers: { 'x-admin': 'true' },
  streamUrl: `${base}/stream`,
  wsUrl: `${base}/ws`,
  defineClientActions: {
    showToast: (toast) => {
      console.log('  server asked showToast:', toast.text)
      return 'shown'
    },
  },
})

console.log('== HTTP unary: greet ==')
const g = await client.fetch({ greet: { name: 'sam', userId: 7 } })
if (isOk(g.greet)) console.log('  greet ->', g.greet.data.message)

console.log('== HTTP unary: validation error ==')
const v = await client.fetch({ greet: { name: '' } })
if (isError(v.greet)) console.log('  greet("") -> error', v.greet.error.code, v.greet.error.message)

console.log('== HTTP: MANY actions in ONE request ==')
const many = await client.fetch({ greet: { name: 'alice', userId: 1 }, version: null })
if (isOk(many.greet)) console.log('  greet   ->', many.greet.data.message)
if (isOk(many.version)) console.log('  version ->', many.version.data)

console.log('== HTTP files: save_image (multipart) ==')
const f = await client.fetch(
  { save_image: { fileName: 'demo.png' } },
  { files: [new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10])] },
)
if (isOk(f.save_image)) console.log('  save_image ->', f.save_image.data)

console.log('== HTTP stream: count ==')
for await (const frame of client.stream({ count: { n: 4 } })) {
  if (isOk(frame.count)) console.log('  count item ->', frame.count.data)
}

console.log('== WS: progress + server->client client_call(showToast) ==')
const conn = client.connect()
await conn.send({ progress: { n: 3 } })
let results = 0
for await (const frame of conn.stream) {
  if ('progress' in frame) {
    const result = (frame as Record<string, { status: string; data: unknown }>).progress
    console.log('  progress item ->', result.data)
    results++
  }
  if (results >= 3) break
}
conn.close()
console.log('== done ==')
