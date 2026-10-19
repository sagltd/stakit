import { expect, spyOn, test } from 'bun:test'

import { createClient } from './index'

interface TestRouter {
  serverActions: { value: { params: null; result: unknown; kind: 'stream' } }
  clientActions: Record<string, never>
}

async function consume(wire: Uint8Array, chunkSize: number) {
  let offset = 0
  let isCancelled = false
  const client = createClient<TestRouter>({
    url: 'https://offline.test/stream',
    fetch: async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          pull(controller) {
            if (offset >= wire.length) return
            const end = Math.min(offset + chunkSize, wire.length)
            controller.enqueue(wire.subarray(offset, end))
            offset = end
          },
          cancel() {
            isCancelled = true
          },
        }),
      ),
  })
  const values: unknown[] = []
  for await (const frame of client.stream({ value: null })) {
    if (frame.value.status !== 'ok')
      throw new Error('unexpected action failure')
    values.push(frame.value.data)
  }
  return { values, isCancelled }
}

test('fragmented large frames search each received character a bounded number of times', async () => {
  const payload = 'x'.repeat(64 * 1024)
  const wire = new TextEncoder().encode(
    `${JSON.stringify({ type: 'next', data: payload })}\n{"type":"end"}\n`,
  )
  const original = String.prototype.indexOf
  let searchWork = 0
  const search = spyOn(String.prototype, 'indexOf').mockImplementation(
    function (this: string, needle: string, position = 0) {
      const result = original.call(this, needle, position)
      if (needle === '\n')
        searchWork += (result < 0 ? this.length : result + 1) - position
      return result
    },
  )
  try {
    const result = await consume(wire, 256)
    expect(result.values).toEqual([payload])
    expect(result.isCancelled).toBe(true)
    // This is deterministic search work, not a wall-clock timing threshold.
    expect(searchWork).toBeLessThanOrEqual(wire.length * 2)
  } finally {
    search.mockRestore()
  }
})

test('one-byte UTF-8 chunks and multiple frames retain exact values and cleanup', async () => {
  const values = ['你好 مرحبا 🙂', null, 0, '', { text: 'é<&>' }]
  const lines = values.map((data) => JSON.stringify({ type: 'next', data }))
  const wire = new TextEncoder().encode(
    `\n{"type":"heartbeat"}\n${lines.join('\n')}\n{"type":"end"}\n`,
  )
  const result = await consume(wire, 1)
  expect(result.values).toEqual(values)
  expect(result.isCancelled).toBe(true)
})

test('multiple frames in one chunk retain exact values and cleanup', async () => {
  const wire = new TextEncoder().encode(
    '{"type":"next","data":1}\n{"type":"next","data":2}\n{"type":"end"}\n',
  )
  const result = await consume(wire, wire.length)
  expect(result.values).toEqual([1, 2])
  expect(result.isCancelled).toBe(true)
})
