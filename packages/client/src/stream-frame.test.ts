import { expect, test } from 'bun:test'

import { createClient, TransportError } from './index'

interface TestRouter {
  serverActions: { value: { params: null; result: unknown; kind: 'stream' } }
  clientActions: Record<string, never>
}

async function consume(wire: string) {
  let isCancelled = false
  const seen: Error[] = []
  const client = createClient<TestRouter>({
    url: 'https://api.example/stream',
    fetch: () =>
      Promise.resolve(
        new Response(
          new ReadableStream({
            start(controller) {
              controller.enqueue(new TextEncoder().encode(wire))
            },
            cancel() {
              isCancelled = true
            },
          }),
        ),
      ),
    onError: (error) => {
      seen.push(error)
      return error
    },
  })
  const values: unknown[] = []
  const failure: unknown = await (async () => {
    for await (const frame of client.stream({ value: null }))
      values.push(frame.value)
  })().catch((error: unknown) => error)
  return { failure, values, seen, isCancelled }
}

for (const frame of [
  'not-json',
  'null',
  '[]',
  '1',
  '{}',
  '{"type":1}',
  '{"type":""}',
  '{"type":"next"}',
  '{"type":"next","data":1,"error":{}}',
  '{"type":"error"}',
  '{"type":"error","error":null}',
  '{"type":"error","error":{"code":"401","message":"bad"}}',
  '{"type":"error","error":{"code":400}}',
  '{"type":"error","error":{"code":400,"message":"bad","type":2}}',
  '{"type":"error","error":{"code":400,"message":"bad","fields":{"name":2}}}',
  '{"type":"error","error":{"code":400,"message":"bad"},"data":1}',
  '{"type":"end","data":1}',
  '{"type":"end","error":{}}',
]) {
  test(`malformed frame rejects through onError and cancels reader: ${frame}`, async () => {
    const result = await consume(`${frame}\n{"type":"end"}\n`)
    expect(result.failure).toBeInstanceOf(TransportError)
    expect(result.failure).toMatchObject({
      status: 200,
      code: 'stream_invalid_frame',
    })
    expect(result.values).toEqual([])
    expect(result.seen).toHaveLength(1)
    expect(result.failure).toBe(result.seen[0])
    expect(result.isCancelled).toBe(true)
  })
}

test('blank, heartbeat and future frame types coexist with null and primitive payloads', async () => {
  const result = await consume(
    '\n \n{"type":"heartbeat"}\n{"type":"future","data":{"extension":1}}\n{"type":"next","data":null}\n{"type":"next","data":0}\n{"type":"next","data":""}\n{"type":"end"}\n',
  )
  expect(result.failure).toBeUndefined()
  expect(result.values).toEqual([
    { status: 'ok', data: null },
    { status: 'ok', data: 0 },
    { status: 'ok', data: '' },
  ])
  expect(result.isCancelled).toBe(true)
})

test('valid application error data and field messages remain action results', async () => {
  const error = {
    code: 422,
    type: 'validation',
    message: 'Fix input',
    fields: { name: ['Required'] },
    data: { hint: true },
  }
  const result = await consume(`${JSON.stringify({ type: 'error', error })}\n`)
  expect(result.failure).toBeUndefined()
  expect(result.values).toEqual([{ status: 'error', error }])
  expect(result.isCancelled).toBe(true)
})
