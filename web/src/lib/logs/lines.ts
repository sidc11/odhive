// What a log view shows of a stream: the rows that pass its filter, with
// their layout, updated incrementally as the stream grows.
import { RowLayout } from './layout'
import type { LogRow } from './rows'

/** The part of `LogStream` a view reads. */
export interface RowSource {
  readonly rows: LogRow[]
  readonly firstId: number
}

export interface SyncResult {
  /** Rows added to the view. */
  added: number
  /** Lines of rows that left the view at the top (dropped by the stream). */
  removedLines: number
}

export class LogLines {
  /** Shown rows, oldest first. */
  items: LogRow[] = []
  readonly layout = new RowLayout()
  /** Id of the newest stream row looked at. */
  private seen = 0
  private filter: (row: LogRow) => boolean = () => true
  /** Ids of shown rows matching the search, ascending. */
  matches: number[] = []
  private search: ((row: LogRow) => boolean) | null = null
  private readonly source: RowSource

  constructor(source: RowSource) {
    this.source = source
  }

  /** Shows only rows passing `filter` (rebuilds the view). */
  setFilter(filter: (row: LogRow) => boolean): void {
    this.filter = filter
    this.items = []
    this.layout.reset()
    this.seen = 0
    this.matches = []
    this.sync()
  }

  /** Tracks shown rows matching `search` (null: none). */
  setSearch(search: ((row: LogRow) => boolean) | null): void {
    this.search = search
    this.matches = search ? this.items.filter(search).map((row) => row.id) : []
  }

  /**
   * Match after (`forward`) or before row id `from`, wrapping around;
   * `inclusive` also accepts `from` itself. Null without matches.
   */
  nextMatch(from: number, forward: boolean, inclusive = false): number | null {
    const matches = this.matches
    if (matches.length === 0) return null
    // Split the matches into "before" and "after" `from`; where `from`
    // itself goes decides whether it can be the answer.
    const fromIsBefore = forward ? !inclusive : inclusive
    let low = 0
    let high = matches.length
    while (low < high) {
      const mid = (low + high) >> 1
      const id = matches[mid] ?? 0
      if (id < from || (id === from && fromIsBefore)) low = mid + 1
      else high = mid
    }
    if (forward) return matches[low] ?? matches[0] ?? null
    return matches[low - 1] ?? matches.at(-1) ?? null
  }

  /** Index of the first shown row with an id at or after `id`. */
  indexOfId(id: number): number {
    let low = 0
    let high = this.items.length
    while (low < high) {
      const mid = (low + high) >> 1
      if ((this.items[mid]?.id ?? 0) < id) low = mid + 1
      else high = mid
    }
    return low
  }

  /** Takes what the stream dropped and added since the last call. */
  sync(): SyncResult {
    const { rows, firstId } = this.source
    // Rows the stream dropped.
    let drop = 0
    while (drop < this.items.length && (this.items[drop]?.id ?? firstId) < firstId) drop += 1
    let removedLines = 0
    if (drop > 0) {
      this.items.splice(0, drop)
      removedLines = this.layout.dropFront(drop)
      const first = this.items[0]?.id ?? Infinity
      let gone = 0
      while (gone < this.matches.length && (this.matches[gone] ?? 0) < first) gone += 1
      if (gone > 0) this.matches.splice(0, gone)
    }
    // Rows it added: ids are consecutive from `firstId`.
    const from = Math.max(this.seen + 1 - firstId, 0)
    const added: LogRow[] = []
    for (let i = from; i < rows.length; i += 1) {
      const row = rows[i]
      if (row && this.filter(row)) added.push(row)
    }
    this.seen = rows.at(-1)?.id ?? this.seen
    for (const row of added) this.items.push(row)
    this.layout.append(added.map((row) => row.lines))
    const search = this.search
    if (search) for (const row of added) if (search(row)) this.matches.push(row.id)
    return { added: added.length, removedLines }
  }
}
