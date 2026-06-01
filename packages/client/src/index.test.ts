import { expect, test } from 'bun:test'

import { STAKIT_CLIENT_VERSION } from './index'

test('exposes a version', () => {
  expect(STAKIT_CLIENT_VERSION).toBe('0.1.0')
})
