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
    this.sync()
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
    return { added: added.length, removedLines }
  }
}
