// Log records and a fake connection for log viewer tests.
import type { ClientMessage, LogRecord, ServerMessage } from '../api'
import type { LiveStatus } from '../live.svelte'
import type { LogConnection } from './stream.svelte'

export function record(seq: number, message = `entry ${seq}`, extra: Partial<LogRecord> = {}): LogRecord {
  return {
    seq,
    message,
    level: 'info',
    logger: 'odoo.modules.loading',
    db: null,
    pid: 4242,
    timestamp: '2026-11-24T09:00:00.000Z',
    exceptions: [],
    locations: [],
    truncated: false,
    ...extra,
  }
}

export function records(from: number, to: number): LogRecord[] {
  return Array.from({ length: to - from + 1 }, (_, i) => record(from + i))
}

export class FakeConnection implements LogConnection {
  status: LiveStatus = 'open'
  sent: ClientMessage[] = []
  private open = new Set<() => void>()
  private message = new Set<(message: ServerMessage) => void>()

  send(message: ClientMessage): boolean {
    if (this.status !== 'open') return false
    this.sent.push(message)
    return true
  }

  onopen(listener: () => void): () => void {
    this.open.add(listener)
    return () => this.open.delete(listener)
  }

  onmessage(listener: (message: ServerMessage) => void): () => void {
    this.message.add(listener)
    return () => this.message.delete(listener)
  }

  /** Simulates a (re)connect. */
  reopen(): void {
    this.status = 'open'
    for (const listener of [...this.open]) listener()
  }

  receive(message: ServerMessage): void {
    for (const listener of [...this.message]) listener(message)
  }

  get listeners(): number {
    return this.open.size + this.message.size
  }
}

const target = { project: 'shop', instance: 'main' } as const

export const msg = {
  subscribed: (next: number, running = true): ServerMessage => ({ type: 'subscribed', ...target, next, running }),
  logs: (list: LogRecord[]): ServerMessage => ({
    type: 'logs',
    ...target,
    records: list,
    next: (list.at(-1)?.seq ?? 0) + 1,
  }),
  gap: (skipped: number): ServerMessage => ({ type: 'gap', ...target, skipped }),
  reset: (): ServerMessage => ({ type: 'reset', ...target }),
}
