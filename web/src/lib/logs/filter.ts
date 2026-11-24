// Filters and search of the log viewer: which rows are shown (levels,
// loggers) and which shown rows match the search (text or regex).
import type { LogRecord } from '../api'
import type { LogRow } from './rows'

/** Level chips; RUNBOT entries and lines without a header go with INFO. */
export const LEVEL_CHIPS = ['debug', 'info', 'warning', 'error', 'critical'] as const
export type LevelChip = (typeof LEVEL_CHIPS)[number]

/** Chip labels: the ones the rows show. */
export const LEVEL_CHIP_LABELS: Record<LevelChip, string> = {
  debug: 'DEBUG',
  info: 'INFO',
  warning: 'WARN',
  error: 'ERROR',
  critical: 'CRIT',
}

/** Longest search a user can type (a regex runs on every row). */
export const MAX_QUERY = 500

export function chipOf(level: LogRecord['level']): LevelChip {
  return level === null || level === undefined || level === 'runbot' ? 'info' : level
}

export interface LoggerFilter {
  include: string[]
  exclude: string[]
}

/**
 * Logger filter text: words separated by commas or spaces, each matched
 * case-insensitively as part of the logger name; a word starting with `-`
 * hides those loggers instead. With include words, an entry must match one.
 */
export function parseLoggers(text: string): LoggerFilter {
  const include: string[] = []
  const exclude: string[] = []
  for (const word of text.toLowerCase().split(/[\s,]+/)) {
    if (word.startsWith('-')) {
      if (word.length > 1) exclude.push(word.slice(1))
    } else if (word) {
      include.push(word)
    }
  }
  return { include, exclude }
}

/** Rows shown with these levels and logger filter; markers always are. */
export function rowFilter(levels: ReadonlySet<LevelChip>, loggers: LoggerFilter): (row: LogRow) => boolean {
  const all = LEVEL_CHIPS.every((chip) => levels.has(chip))
  if (all && loggers.include.length === 0 && loggers.exclude.length === 0) return () => true
  return (row) => {
    if (row.kind !== 'entry') return true
    const { level, logger } = row.record
    if (!all && !levels.has(chipOf(level))) return false
    const name = (logger ?? '').toLowerCase()
    if (loggers.exclude.some((word) => name.includes(word))) return false
    return loggers.include.length === 0 || loggers.include.some((word) => name.includes(word))
  }
}

export interface SearchOptions {
  regex: boolean
  caseSensitive: boolean
}

export type Search = { ok: true; pattern: RegExp | null } | { ok: false; error: string }

function escapeRegex(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

/**
 * Compiles a search; `pattern` is null for an empty one. Plain text is
 * matched literally. The pattern is global (for highlighting).
 */
export function compileSearch(query: string, { regex, caseSensitive }: SearchOptions): Search {
  if (query === '') return { ok: true, pattern: null }
  if (query.length > MAX_QUERY)
    return {
      ok: false,
      error: `Search is longer than ${MAX_QUERY} characters`,
    }
  const flags = caseSensitive ? 'g' : 'gi'
  try {
    return {
      ok: true,
      pattern: new RegExp(regex ? query : escapeRegex(query), flags),
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    return {
      ok: false,
      error: message.replace(/^Invalid regular expression: /, ''),
    }
  }
}

/** Whether a row matches the search: a non-empty match in its logger or message. */
export function rowMatcher(pattern: RegExp): (row: LogRow) => boolean {
  const test = new RegExp(pattern.source, pattern.flags.replace('g', ''))
  const hit = (text: string | null | undefined): boolean => {
    if (!text) return false
    const found = test.exec(text)
    if (found === null) return false
    if (found[0] !== '') return true
    // An empty match (`x*`): look for a real one.
    return firstSegmentMatch(text, pattern)
  }
  return (row) => row.kind === 'entry' && (hit(row.record.message) || hit(row.record.logger))
}

function firstSegmentMatch(text: string, pattern: RegExp): boolean {
  for (const found of text.matchAll(pattern)) if (found[0] !== '') return true
  return false
}

export interface Segment {
  text: string
  match: boolean
}

/** Most highlighted matches per field (keeps huge lines cheap). */
const MAX_MARKS = 1000

/** Splits `text` into matching and non-matching parts. */
export function highlight(text: string, pattern: RegExp | null): Segment[] {
  if (!pattern || text === '') return [{ text, match: false }]
  const segments: Segment[] = []
  let last = 0
  let marks = 0
  for (const found of text.matchAll(pattern)) {
    const start = found.index
    const match = found[0]
    if (match === '') continue
    if (start > last) segments.push({ text: text.slice(last, start), match: false })
    segments.push({ text: match, match: true })
    last = start + match.length
    marks += 1
    if (marks >= MAX_MARKS) break
  }
  if (last < text.length) segments.push({ text: text.slice(last), match: false })
  return segments
}
