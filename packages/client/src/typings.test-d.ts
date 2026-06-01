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

void _guards
void _params
void _resultMap
