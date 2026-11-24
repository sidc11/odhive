// The live log of one instance, kept in the browser: entries from the
// WebSocket plus markers for gaps and restarts, oldest dropped first.
import type { ClientMessage, LogRecord, ServerMessage } from '../api'
import type { LiveStatus } from '../live.svelte'
import { entryLines, type LogRow } from './rows'

/** The part of `LiveConnection` a stream uses. */
export interface LogConnection {
  readonly status: LiveStatus
  send(message: ClientMessage): boolean
  onopen(listener: () => void): () => void
  onmessage(listener: (message: ServerMessage) => void): () => void
}

/** Newest entries asked for when a stream starts (the server's maximum). */
export const TAIL = 5000
/** Rows kept in the browser; past this the oldest go until 90% are left. */
export const MAX_ROWS = 200_000

export interface StreamOptions {
  tail?: number
  maxRows?: number
}

export class LogStream {
  readonly project: string
  readonly instance: string

  /** Bumped whenever `rows` changes. */
  version = $state(0)
  /** The service runs the instance (from the last `subscribed`); `null` before. */
  running = $state<boolean | null>(null)

  /**
   * Rows, oldest first. Plain (not reactive): with 100k+ entries a deep
   * proxy costs too much; read `version` to depend on it.
   */
  rows: LogRow[] = []
  /** Rows dropped from the front so far (ids of `rows` start after them). */
  dropped = 0

  private nextId = 1
  /** Sequence number of the last entry of the current log; 0 = none. */
  private lastSeq = 0
  /** Subscriptions sent and not answered yet: their messages are stale. */
  private awaiting = 0
  /** Resubscribed with `from` = `lastSeq`: the first entry must match ours. */
  private verifying = false
  /** The next rows are of a new log: start them with a marker. */
  private pendingReset = false
  private offs: (() => void)[] = []
  private readonly connection: LogConnection
  private readonly tail: number
  private readonly maxRows: number

  constructor(connection: LogConnection, project: string, instance: string, options: StreamOptions = {}) {
    this.connection = connection
    this.project = project
    this.instance = instance
    this.tail = options.tail ?? TAIL
    this.maxRows = Math.max(options.maxRows ?? MAX_ROWS, 10)
  }

  /** Id of `rows[0]`. */
  get firstId(): number {
    return this.dropped + 1
  }

  start(): void {
    if (this.offs.length > 0) return
    this.offs = [
      this.connection.onopen(() => this.subscribe()),
      this.connection.onmessage((message) => this.apply(message)),
    ]
    if (this.connection.status === 'open') this.subscribe()
  }

  stop(): void {
    for (const off of this.offs) off()
    this.offs = []
    this.awaiting = 0
    this.connection.send({ op: 'unsubscribe_logs', project: this.project, instance: this.instance })
  }

  /** Subscribes: the newest entries at first, else from where we are. */
  private subscribe(fresh = this.lastSeq === 0): void {
    const base = { op: 'subscribe_logs', project: this.project, instance: this.instance } as const
    const message: ClientMessage = fresh ? { ...base, tail: this.tail } : { ...base, from: this.lastSeq }
    this.verifying = !fresh
    if (this.connection.send(message)) this.awaiting += 1
  }

  /** The log we had is gone: take the new one from its start. */
  private restartLog(): void {
    this.pendingReset = this.rows.length > 0
    this.lastSeq = 0
    this.subscribe(true)
  }

  apply(message: ServerMessage): void {
    if (!('project' in message) || message.project !== this.project || message.instance !== this.instance) return
    switch (message.type) {
      case 'subscribed':
        this.awaiting = Math.max(this.awaiting - 1, 0)
        if (this.awaiting > 0) return
        this.running = message.running
        // Not running here: the log we followed ended with the service
        // that had it, and `from` would skip the next log's first entries.
        if (this.verifying && !message.running) this.restartLog()
        return
      case 'logs':
        if (this.awaiting > 0) return
        this.addRecords(message.records)
        return
      case 'gap':
        if (this.awaiting > 0) return
        // Our last entry left memory: assume it is the same log.
        this.verifying = false
        this.push([{ id: 0, kind: 'gap', skipped: message.skipped, lines: 1 }])
        return
      case 'reset':
        if (this.awaiting > 0) return
        this.verifying = false
        this.lastSeq = 0
        this.pendingReset = this.rows.length > 0
        return
    }
  }

  private addRecords(records: LogRecord[]): void {
    let list = records
    if (this.verifying) {
      this.verifying = false
      const first = list[0]
      const last = this.lastRecord()
      const same =
        first !== undefined &&
        last !== undefined &&
        first.seq === last.seq &&
        first.timestamp === last.timestamp &&
        first.message === last.message
      if (!same) {
        this.restartLog()
        return
      }
      list = list.slice(1)
    }
    if (list.length === 0) return
    this.lastSeq = list.at(-1)?.seq ?? this.lastSeq
    this.push(list.map((record) => ({ id: 0, kind: 'entry', record, lines: entryLines(record) })))
  }

  private lastRecord(): LogRecord | undefined {
    for (let i = this.rows.length - 1; i >= 0; i -= 1) {
      const row = this.rows[i]
      if (row?.kind === 'entry') return row.record
      if (row?.kind === 'reset') return undefined
    }
    return undefined
  }

  private push(rows: LogRow[]): void {
    if (this.pendingReset) {
      this.pendingReset = false
      rows.unshift({ id: 0, kind: 'reset', lines: 1 })
    }
    for (const row of rows) {
      row.id = this.nextId
      this.nextId += 1
      this.rows.push(row)
    }
    if (this.rows.length > this.maxRows) {
      const drop = this.rows.length - this.maxRows + Math.floor(this.maxRows / 10)
      this.rows.splice(0, drop)
      this.dropped += drop
    }
    this.version += 1
  }
}
