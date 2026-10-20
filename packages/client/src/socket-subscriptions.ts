// Channel subscriptions over one socket: one server subscription per
// (channel, key) shared by every local `Subscription`, gap detection
// (seq / epoch / reset / resubscribe), frames that arrive before their ack
// buffered and replayed, re-subscribe after reconnect or a server reset.
// Semantics: docs/pubsub.md.

import { readAck, readError, stableKey } from './socket-frames'
import type { Frame } from './socket-frames'
import type { ErrorBody } from './types'

/** One message published on a subscribed channel. */
export interface PublishMessage {
  readonly topic: string
  /** The client action name (its publish handler, if any, already ran). */
  readonly name: string
  readonly params: unknown
  readonly seq: number
  readonly epoch: number
}

/** Why a subscription may have missed messages — resync from your source. */
export type GapReason = 'seq' | 'epoch' | 'reset' | 'resubscribe'

/** A live channel subscription; survives reconnects. */
export interface Subscription {
  /** The server topic, once the first subscribe was acknowledged. */
  readonly topic: string | undefined
  onMessage?: (message: PublishMessage) => void
  /** Messages may have been missed: refetch the state this channel tracks. */
  onGap?: (reason: GapReason) => void
  /** The server rejected the subscription (e.g. `403`); it is dropped. */
  onError?: (error: ErrorBody) => void
  /** The server removed the subscription (e.g. membership revoked). */
  onKick?: () => void
  readonly unsubscribe: () => void
}

export type SubscriptionHandlers = Pick<
  Subscription,
  'onMessage' | 'onGap' | 'onError' | 'onKick'
>

interface Sub extends Subscription {
  topic: string | undefined
}

interface Group {
  readonly channel: string
  readonly key: unknown
  readonly id: string
  readonly subs: Set<Sub>
  topic?: string
  epoch?: number
  seq?: number
  pending: boolean
}

/** Frames buffered per topic while a subscribe ack is pending. */
const EARLY_PER_TOPIC = 64

export interface SubscriptionDeps {
  readonly isReady: () => boolean
  /** Sends a subscribe; `onFrame` gets its single `result`. */
  readonly subscribe: (
    channel: string,
    key: unknown,
    onFrame: (frame: Frame) => void,
  ) => void
  readonly unsubscribe: (topic: string) => void
  /** Runs user code, reporting (not throwing) its errors. */
  readonly safe: (run: () => unknown) => void
  /** A publish frame passed the topic check (runs publish handlers). */
  readonly delivered: (frame: Frame) => void
}

export function createSubscriptions(deps: SubscriptionDeps) {
  const groups = new Map<string, Group>()
  const byTopic = new Map<string, Group>()
  const early = new Map<string, Frame[]>()

  const each = (group: Group, run: (sub: Sub) => void): void => {
    for (const sub of [...group.subs]) deps.safe(() => run(sub))
  }

  const drop = (group: Group): void => {
    groups.delete(group.id)
    if (group.topic !== undefined && byTopic.get(group.topic) === group)
      byTopic.delete(group.topic)
  }

  const send = (group: Group): void => {
    group.pending = true
    deps.subscribe(group.channel, group.key, (frame) => onAck(group, frame))
  }

  function onAck(group: Group, frame: Frame): void {
    group.pending = false
    if (groups.get(group.id) !== group) return
    if (frame.status === 'error') {
      drop(group)
      const error = readError(frame.error)
      each(group, (sub) => sub.onError?.(error))
      return
    }
    const ack = readAck(frame.data)
    if (!ack) return drop(group)
    if (group.subs.size === 0) {
      // Everyone left before the ack: release the server subscription.
      drop(group)
      if (deps.isReady()) deps.unsubscribe(ack.topic)
      return
    }
    const moved =
      group.epoch !== undefined &&
      (ack.epoch !== group.epoch || ack.seq !== group.seq)
    group.topic = ack.topic
    group.epoch = ack.epoch
    group.seq = ack.seq
    byTopic.set(ack.topic, group)
    for (const sub of group.subs) sub.topic = ack.topic
    if (moved) each(group, (sub) => sub.onGap?.('resubscribe'))
    const buffered = early.get(ack.topic) ?? []
    early.delete(ack.topic)
    for (const frame of buffered)
      if (frame.epoch === ack.epoch && (frame.seq ?? 0) > ack.seq)
        onPublish(frame)
  }

  /** A publish frame: dispatch, gap-check, deliver; buffer if not acked yet. */
  function onPublish(frame: Frame): void {
    const topic = frame.topic ?? ''
    const group = byTopic.get(topic)
    if (!group) {
      if (topic.startsWith('user:') || topic.startsWith('device:'))
        return deps.delivered(frame)
      if (![...groups.values()].some((pending) => pending.pending)) return
      const list = early.get(topic) ?? []
      if (list.length < EARLY_PER_TOPIC) early.set(topic, [...list, frame])
      return
    }
    const seq = frame.seq ?? 0
    const epoch = frame.epoch ?? 0
    const gap: GapReason | null =
      epoch !== group.epoch
        ? 'epoch'
        : seq !== (group.seq ?? 0) + 1
          ? 'seq'
          : null
    // State first: a throwing callback can't desync the counters.
    group.epoch = epoch
    group.seq = seq
    deps.delivered(frame)
    const message = {
      topic,
      name: frame.name ?? '',
      params: frame.params,
      seq,
      epoch,
    }
    each(group, (sub) => {
      if (gap) sub.onGap?.(gap)
      sub.onMessage?.(message)
    })
  }

  function onReset(frame: Frame): void {
    const group = byTopic.get(frame.topic ?? '')
    if (!group || frame.epoch === undefined) return
    group.epoch = frame.epoch
    group.seq = frame.seq ?? 0
    each(group, (sub) => sub.onGap?.('reset'))
  }

  function onUnsubscribed(frame: Frame): void {
    const group = byTopic.get(frame.topic ?? '')
    if (!group) return
    byTopic.delete(group.topic ?? '')
    if (frame.reason === 'reset') {
      // The server dropped it to re-check access: subscribe again.
      each(group, (sub) => sub.onGap?.('reset'))
      if (deps.isReady()) send(group)
      return
    }
    groups.delete(group.id)
    each(group, (sub) => sub.onKick?.())
  }

  function subscribe(
    channel: string,
    key: unknown,
    handlers: SubscriptionHandlers = {},
  ): Subscription {
    const id = `${channel}\u0000${stableKey(key)}`
    let group = groups.get(id)
    const isNew = !group
    if (!group) {
      group = { channel, key, id, subs: new Set(), pending: false }
      groups.set(id, group)
    }
    const owner = group
    const sub: Sub = {
      topic: owner.topic,
      ...handlers,
      unsubscribe: () => {
        if (!owner.subs.delete(sub) || owner.subs.size > 0) return
        if (owner.pending) return // the late ack releases it
        drop(owner)
        if (owner.topic !== undefined && deps.isReady())
          deps.unsubscribe(owner.topic)
      },
    }
    owner.subs.add(sub)
    if (isNew && deps.isReady()) send(owner)
    return sub
  }

  return {
    subscribe,
    onPublish,
    onReset,
    onUnsubscribed,
    /** Connected and authenticated: (re)subscribe every live group. */
    resubscribe(): void {
      for (const group of groups.values())
        if (group.subs.size === 0) groups.delete(group.id)
        else send(group)
    },
    /** The socket is gone: server-side state with it. */
    disconnected(): void {
      byTopic.clear()
      early.clear()
      for (const group of groups.values()) group.pending = false
    },
    clear(): void {
      groups.clear()
      byTopic.clear()
      early.clear()
    },
  }
}
