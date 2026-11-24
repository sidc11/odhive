// Tracebacks in log entries: folding, copying, and `file:line` links.
import type { LogRecord } from '../api'
import { highlight, type Segment } from './filter'
import type { LogRow } from './rows'

/** Python's first traceback line. */
export const TRACEBACK_START = 'Traceback (most recent call last):'

/** Whether an entry is a traceback that folds to one line. */
export function foldable(row: LogRow): boolean {
  return row.kind === 'entry' && row.lines > 1 && row.record.exceptions.length > 0
}

/** Lines a row takes on screen, folded or not. */
export function shownLines(row: LogRow, expanded: boolean): number {
  return foldable(row) && !expanded ? 1 : row.lines
}

/**
 * The traceback of an entry, exactly as logged: from its first
 * `Traceback (most recent call last):` line to the end of the message
 * (chained tracebacks included); the whole message if that line is missing.
 */
export function tracebackText(message: string): string {
  let start = 0
  for (;;) {
    const end = message.indexOf('\n', start)
    const line = end === -1 ? message.slice(start) : message.slice(start, end)
    if (line.trim() === TRACEBACK_START) return message.slice(start)
    if (end === -1) return message
    start = end + 1
  }
}

/** First line of a message. */
export function firstLine(message: string): string {
  const end = message.indexOf('\n')
  return end === -1 ? message : message.slice(0, end)
}

/** What a folded entry shows after its first line: the exception and the hidden line count. */
export function foldSummary(record: Pick<LogRecord, 'message' | 'exceptions'>, lines: number): string {
  const exception = record.exceptions.at(-1) ?? ''
  const hidden = lines - 1
  const count = `${hidden.toLocaleString()} more ${hidden === 1 ? 'line' : 'lines'}`
  return exception && exception !== firstLine(record.message) ? `${exception} · ${count}` : count
}

/** A source location in a message, by character offsets. */
export interface FileLink {
  start: number
  end: number
  path: string
  line: number
}

// `File "/abs/path.py", line 12` (a Python frame) and Odoo's
// `ParseError: while parsing /abs/path.xml:12`. Only absolute paths.
const FRAME = /File "(\/[^"\n]+)", line (\d+)/g
const XML = /while parsing (\/[^\n]*?):(\d+)$/gm
/** Most links per message. */
const MAX_LINKS = 500

/** Source locations in `message` that can be opened, in order. */
export function fileLinks(message: string): FileLink[] {
  const links: FileLink[] = []
  for (const pattern of [FRAME, XML]) {
    for (const found of message.matchAll(pattern)) {
      const path = found[1] ?? ''
      const line = Number(found[2])
      if (!Number.isSafeInteger(line) || line < 1) continue
      const start = found.index + found[0].indexOf(path)
      links.push({ start, end: start + path.length, path, line })
      if (links.length >= MAX_LINKS) break
    }
  }
  return links.sort((a, b) => a.start - b.start)
}

/** A run of text that is one link, or plain text. */
export interface Piece {
  link: FileLink | null
  parts: Segment[]
}

/** Cuts highlight `segments` (covering a text) to the range [from, to). */
function cut(segments: Segment[], from: number, to: number): Segment[] {
  const out: Segment[] = []
  let offset = 0
  for (const segment of segments) {
    const start = offset
    const end = offset + segment.text.length
    offset = end
    if (end <= from) continue
    if (start >= to) break
    out.push({ text: segment.text.slice(Math.max(from - start, 0), Math.min(to, end) - start), match: segment.match })
  }
  return out
}

/** `text` split into links and plain runs, each highlighted by `pattern`. */
export function decorate(text: string, pattern: RegExp | null, links: FileLink[]): Piece[] {
  const segments = highlight(text, pattern)
  if (links.length === 0) return [{ link: null, parts: segments }]
  const pieces: Piece[] = []
  let last = 0
  for (const link of links) {
    if (link.start < last || link.end > text.length) continue
    if (link.start > last) pieces.push({ link: null, parts: cut(segments, last, link.start) })
    pieces.push({ link, parts: cut(segments, link.start, link.end) })
    last = link.end
  }
  if (last < text.length) pieces.push({ link: null, parts: cut(segments, last, text.length) })
  return pieces
}
