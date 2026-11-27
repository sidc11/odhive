// The PostgreSQL server log of a project, polled from the API and kept in
// the browser like an instance log (oldest rows dropped first).
import type { PostgresLog } from '../api'
import type { LogFeed } from './lines'
import { entryLines, type LogRow } from './rows'
import { MAX_ROWS, TAIL } from './stream.svelte'

/** How often the log is read (ms). */
export const POLL_INTERVAL = 1000

/** Reads the log: the newest entries without `from`, else from `from` on. */
export type PostgresLogFetch = (query: { from?: number; limit?: number }) => Promise<PostgresLog>

export interface PostgresStreamOptions {
  interval?: number
  maxRows?: number
  tail?: number
  /** Whether the page is hidden (no reads then). */
  hidden?: () => boolean
}

/** What the last answer said about the log, without its entries. */
export type PostgresLogInfo = Omit<PostgresLog, 'records'>

export class PostgresLogStream implements LogFeed {
  version = $state(0)
  running = $state<boolean | null>(null)
  /** The last answer; `null` before the first. */
  info = $state.raw<PostgresLogInfo | null>(null)
  /** Why the last read failed (network, server), if it did. */
  error = $state<string | null>(null)

  rows: LogRow[] = []
  dropped = 0

  private nextId = 1
  /** Stream and `next` of the last answer; `stream` 0 = none. */
  private stream = 0
  private next = 1
  private path: string | null = null
  private timer: ReturnType<typeof setTimeout> | null = null
  private active = false
  /** Bumped by `restart`: answers to older reads are dropped. */
  private generation = 0
  private readonly fetch: PostgresLogFetch
  private readonly interval: number
  private readonly maxRows: number
  private readonly tail: number
  private readonly hidden: () => boolean

  constructor(fetch: PostgresLogFetch, options: PostgresStreamOptions = {}) {
    this.fetch = fetch
    this.interval = options.interval ?? POLL_INTERVAL
    this.maxRows = Math.max(options.maxRows ?? MAX_ROWS, 10)
    this.tail = options.tail ?? Math.min(TAIL, 1000)
    this.hidden = options.hidden ?? (() => typeof document !== 'undefined' && document.hidden)
  }

  get firstId(): number {
    return this.dropped + 1
  }

  start(): void {
    if (this.active) return
    this.active = true
    void this.poll()
  }

  stop(): void {
    this.active = false
    if (this.timer !== null) clearTimeout(this.timer)
    this.timer = null
  }

  /** Starts over with the newest entries, e.g. after another file was chosen. */
  restart(): void {
    this.generation += 1
    this.stream = 0
    this.clear()
    if (this.active) {
      if (this.timer !== null) clearTimeout(this.timer)
      this.timer = null
      void this.poll()
    }
  }

  /** Takes an answer got elsewhere (e.g. from saving the source). */
  accept(log: PostgresLog): void {
    this.apply(log, false)
  }

  /** One read, then the next one is scheduled. */
  async poll(): Promise<void> {
    const generation = this.generation
    if (!this.hidden()) {
      const asked = this.stream
      try {
        const log = await this.fetch(asked === 0 ? { limit: this.tail } : { from: this.next })
        if (generation === this.generation) {
          this.error = null
          // Read anew on the server: `from` meant nothing there.
          if (asked !== 0 && log.stream !== asked) {
            this.stream = 0
            if (this.active) this.schedule(0)
            return
          }
          this.apply(log, asked !== 0)
        }
      } catch (error) {
        if (generation === this.generation) this.error = error instanceof Error ? error.message : String(error)
      }
    }
    if (this.active && generation === this.generation) this.schedule(this.interval)
  }

  private schedule(delay: number): void {
    if (this.timer !== null) clearTimeout(this.timer)
    this.timer = setTimeout(() => {
      this.timer = null
      void this.poll()
    }, delay)
  }

  private apply(log: PostgresLog, continued: boolean): void {
    const { records, ...info } = log
    this.info = info
    this.running = log.status === 'ok'
    if (log.status !== 'ok') {
      this.stream = 0
      this.path = null
      this.clear()
      return
    }
    const path = log.path ?? null
    const fresh = !continued || path !== this.path
    if (fresh) {
      // A new file or a new read of it: what we had is replaced.
      this.clear()
      this.path = path
    }
    this.stream = log.stream
    this.next = log.next
    const rows: LogRow[] = []
    if (log.skipped > 0) rows.push({ id: 0, kind: 'gap', skipped: log.skipped, lines: 1 })
    for (const record of records) {
      rows.push({ id: 0, kind: 'entry', record, lines: entryLines(record) })
    }
    if (rows.length > 0 || fresh) this.push(rows)
  }

  private clear(): void {
    if (this.rows.length === 0) return
    this.dropped += this.rows.length
    this.rows = []
    this.version += 1
  }

  private push(rows: LogRow[]): void {
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
