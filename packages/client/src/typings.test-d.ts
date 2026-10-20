// Type-level tests — checked by `tsc --noEmit`, never executed (the `.test-d.ts`
// suffix keeps the bun runtime runner from picking it up). A regression here is
// a compile error. The functions below are declared but intentionally not run.

import { createClient, isError, isOk } from './index'
import type { ActionResult } from './index'

interface TestRouter {
  serverActions: {
    greet: {
      params: { name: string }
      result: { message: string }
      kind: 'unary'
    }
    count: { params: { n: number }; result: number; kind: 'stream' }
  }
  clientActions: {
    showToast: { params: { text: string }; return: string }
  }
  channels: { room: { id: string } }
}

type Expect<T extends true> = T
type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
    ? true
    : false

declare const client: ReturnType<typeof createClient<TestRouter>>

// guards narrow ActionResult to its branch
async function _guards(): Promise<void> {
  const r = await client.fetch({ greet: { name: 'a' } })

  if (isOk(r.greet)) {
    type _D = Expect<Equal<typeof r.greet.data, { message: string }>>
    const _data: _D = true
    void _data
    // @ts-expect-error — error branch is absent after isOk
    void r.greet.error
  }

  if (isError(r.greet)) {
    type _C = Expect<Equal<typeof r.greet.error.code, number>>
    const _code: _C = true
    void _code
    // @ts-expect-error — data branch is absent after isError
    void r.greet.data
  }
}

// params are typed per action
function _params(): void {
  // @ts-expect-error — name must be a string
  void client.fetch({ greet: { name: 123 } })

  // @ts-expect-error — unknown action is rejected
  void client.fetch({ nope: {} })

  void client.fetch({ greet: { name: 'a' } })
}

// result map keyed by the requested actions
async function _resultMap(): Promise<void> {
  const r = await client.fetch({ greet: { name: 'a' } })
  void r
  type _Keys = Expect<Equal<keyof typeof r, 'greet'>>
  type _Val = Expect<
    Equal<(typeof r)['greet'], ActionResult<{ message: string }>>
  >
  const _keys: _Keys = true
  const _val: _Val = true
  void _keys
  void _val
}

/** The shape of `expo/fetch`: its own request and response types, no extras. */
declare function expoLikeFetch(
  input: string | URL | { readonly url: string },
  init?: {
    method?: string
    headers?: Record<string, string> | [string, string][]
    body?: string | ArrayBuffer | Blob | FormData | URLSearchParams | null
    signal?: AbortSignal | null
    credentials?: 'include' | 'omit' | 'same-origin'
  },
): Promise<{
  readonly ok: boolean
  readonly status: number
  readonly headers: Headers
  readonly body: ReadableStream<Uint8Array> | null
  json(): Promise<unknown>
  text(): Promise<string>
  arrayBuffer(): Promise<ArrayBuffer>
}>

// `fetch` takes any fetch-compatible function (e.g. `expo/fetch`), nothing else
function _fetchOption(): void {
  void createClient<TestRouter>({ url: 'http://x', fetch })
  void createClient<TestRouter>({
    url: 'http://x',
    fetch: (input, init) => globalThis.fetch(input, init),
  })
  void createClient<TestRouter>({ url: 'http://x', fetch: expoLikeFetch })

  // @ts-expect-error — fetch must be a fetch-compatible function
  void createClient<TestRouter>({ url: 'http://x', fetch: 'nope' })
}

// GET is the only method that may put params in the url; the type stays open
function _methodOption(): void {
  void client.fetch({ greet: { name: 'a' } }, { method: 'GET' })
  // @ts-expect-error — not an HTTP method the client knows
  void client.fetch({ greet: { name: 'a' } }, { method: 'TRACE' })
}

void _guards
void _params
void _resultMap
void _fetchOption
void _methodOption
void _errorData
void _socket

// socket calls and subscriptions are typed from the generated Router
async function _socket(): Promise<void> {
  const conn = client.connect()
  const reply = await conn.call('greet', { name: 'a' })
  if (isOk(reply)) {
    type _R = Expect<Equal<typeof reply.data, { message: string }>>
    const _result: _R = true
    void _result
  }
  conn.subscribe('room', { id: 'r1' })
  // @ts-expect-error — unknown channel
  conn.subscribe('lobby', { id: 'r1' })
  // @ts-expect-error — wrong key shape
  conn.subscribe('room', { roomId: 'r1' })
  // @ts-expect-error — wrong params
  await conn.call('greet', { nope: 1 })
  client.connect({
    defineClientActions: { showToast: ({ text }) => text },
    publishHandlers: { showToast: ({ text }) => void text },
  })
  // @ts-expect-error — publish handler params are typed
  client.connect({ publishHandlers: { showToast: ({ nope }) => nope } })
  const plain = createClient<{ serverActions: TestRouter['serverActions'] }>({
    url: 'x',
  }).connect()
  // @ts-expect-error — a router without channels has nothing to subscribe to
  plain.subscribe('room', { id: 'r1' })
}

// an error body may carry a structured payload, typed `unknown` until narrowed,
// and a stable machine-readable `type` to branch on
function _errorData(result: ActionResult<number>): void {
  if (isError(result)) {
    type _E = Expect<Equal<typeof result.error.data, unknown>>
    const _payload: _E = true
    void _payload
    type _T = Expect<Equal<typeof result.error.type, string | undefined>>
    const _type: _T = true
    void _type
  }
}
