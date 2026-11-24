// Copying log rows as text: each entry as Odoo wrote it, so a copy pastes
// into a bug report or a log tool like the file itself.
import type { LogRecord } from '../api'
import type { LogRow } from './rows'

const pad = (n: number, width = 2): string => String(n).padStart(width, '0')

/** Odoo's `YYYY-MM-DD HH:MM:SS,mmm` (UTC, as Odoo logs it); null if invalid. */
export function odooTime(timestamp: string): string | null {
  const date = new Date(timestamp)
  if (Number.isNaN(date.getTime())) return null
  const day = `${pad(date.getUTCFullYear(), 4)}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())}`
  const time = `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())}`
  return `${day} ${time},${pad(date.getUTCMilliseconds(), 3)}`
}

/**
 * An entry as a log line: `asctime pid LEVEL db logger: message` when it has
 * a header, else just its message (lines before Odoo set up logging).
 */
export function entryText(record: LogRecord): string {
  const time = record.timestamp ? odooTime(record.timestamp) : null
  if (time === null || !record.level || !record.logger) return record.message
  const pid = record.pid ?? 0
  return `${time} ${pid} ${record.level.toUpperCase()} ${record.db ?? '?'} ${record.logger}: ${record.message}`
}

/** The entries among `rows` as log text, one entry per line (markers are left out). */
export function rowsText(rows: Iterable<LogRow>): string {
  const out: string[] = []
  for (const row of rows) if (row.kind === 'entry') out.push(entryText(row.record))
  return out.join('\n')
}

/** Entries among `rows` (what a copy holds). */
export function entryCount(rows: Iterable<LogRow>): number {
  let count = 0
  for (const row of rows) if (row.kind === 'entry') count += 1
  return count
}

/** Row id of the row element holding `node` inside `container`, if any. */
function rowIdOf(node: Node | null, container: HTMLElement): number | null {
  const element = node instanceof Element ? node : (node?.parentElement ?? null)
  const row = element?.closest<HTMLElement>('[data-row-id]')
  if (!row || !container.contains(row)) return null
  const id = Number(row.dataset['rowId'])
  return Number.isSafeInteger(id) ? id : null
}

/** First and last row ids a text selection touches inside `container`; null if none. */
export function selectedRowIds(
  selection: Selection | null,
  container: HTMLElement | undefined,
): { from: number; to: number } | null {
  if (!selection || !container || selection.isCollapsed || selection.rangeCount === 0) return null
  const range = selection.getRangeAt(0)
  if (!container.contains(range.commonAncestorContainer)) return null
  let from = rowIdOf(range.startContainer, container)
  let to = rowIdOf(range.endContainer, container)
  // An end outside any row (e.g. on the list itself): the rows that hold text of the range.
  if (from === null || to === null) {
    const ids: number[] = []
    for (const row of container.querySelectorAll<HTMLElement>('[data-row-id]')) {
      if (range.intersectsNode(row)) ids.push(Number(row.dataset['rowId']))
    }
    if (ids.length === 0) return null
    from ??= Math.min(...ids)
    to ??= Math.max(...ids)
  }
  return from <= to ? { from, to } : { from: to, to: from }
}
