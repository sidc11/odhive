// Rows of the log viewer: log entries and the markers between them.
import type { LogRecord } from '../api'

export type LogRow =
  | { id: number; kind: 'entry'; record: LogRecord; lines: number }
  | { id: number; kind: 'gap'; skipped: number; lines: 1 }
  | { id: number; kind: 'reset'; lines: 1 }

/** Text lines an entry takes on screen: its message lines (at least one). */
export function entryLines(record: Pick<LogRecord, 'message'>): number {
  let lines = 1
  const message = record.message
  for (let i = message.indexOf('\n'); i !== -1; i = message.indexOf('\n', i + 1)) lines += 1
  return lines
}

const pad = (n: number, width = 2): string => String(n).padStart(width, '0')

/** `HH:MM:SS.mmm` in local time; `''` without a (valid) timestamp. */
export function formatTime(timestamp: string | null | undefined): string {
  if (!timestamp) return ''
  const date = new Date(timestamp)
  if (Number.isNaN(date.getTime())) return ''
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.${pad(date.getMilliseconds(), 3)}`
}

/** Short level label, fixed width in a monospace font. */
export const LEVEL_LABELS = {
  debug: 'DEBUG',
  info: 'INFO',
  runbot: 'RUNBOT',
  warning: 'WARN',
  error: 'ERROR',
  critical: 'CRIT',
} as const
