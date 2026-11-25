// Exporting what a log view shows as a file: `.log` (Odoo's own lines, as
// in the log file), `.txt` (readable, local time, with what was filtered)
// or `.json` (the records themselves).
import type { LogRecord } from '../api'
import { entryText } from './copy'
import { LEVEL_CHIP_LABELS, LEVEL_CHIPS, type LevelChip } from './filter'
import { eventOf } from './marks'
import { LEVEL_LABELS, type LogRow } from './rows'

export const EXPORT_FORMATS = ['log', 'txt', 'json'] as const
export type ExportFormat = (typeof EXPORT_FORMATS)[number]

const MIME: Record<ExportFormat, string> = {
  log: 'text/plain;charset=utf-8',
  txt: 'text/plain;charset=utf-8',
  json: 'application/json;charset=utf-8',
}

/** What the view filtered by, as the user set it. */
export interface ExportFilter {
  levels: ReadonlySet<LevelChip>
  loggers: string
  /** The applied search; `''` for none. */
  search: string
  regex: boolean
  caseSensitive: boolean
  /** Only rows matching the search were exported. */
  onlyMatching: boolean
}

export interface ExportInput {
  project: string
  instance: string
  /** Rows to export (gap/reset markers are kept in `.txt` only). */
  rows: readonly LogRow[]
  filter: ExportFilter
  /** When the export was made. */
  now: Date
}

export interface ExportFile {
  name: string
  type: string
  text: string
  /** Entries in the file. */
  entries: number
}

const pad = (n: number, width = 2): string => String(n).padStart(width, '0')

/** `YYYYMMDD-HHMMSS` in local time, for file names. */
function stamp(date: Date): string {
  const day = `${pad(date.getFullYear(), 4)}${pad(date.getMonth() + 1)}${pad(date.getDate())}`
  return `${day}-${pad(date.getHours())}${pad(date.getMinutes())}${pad(date.getSeconds())}`
}

/** A file name part: letters, digits, `.`, `_` and `-` only (no path, no dot files). */
function safe(part: string): string {
  const cleaned = part.replace(/[^A-Za-z0-9._-]+/g, '_').replace(/^[.]+/, '')
  return cleaned.slice(0, 64) || 'log'
}

/** `<project>-<instance>-<YYYYMMDD-HHMMSS>.<ext>` */
export function exportName(project: string, instance: string, now: Date, format: ExportFormat): string {
  return `${safe(project)}-${safe(instance)}-${stamp(now)}.${format}`
}

/** Local `YYYY-MM-DD HH:MM:SS.mmm`; `''` without a (valid) timestamp. */
export function localTime(timestamp: string | null | undefined): string {
  if (!timestamp) return ''
  const date = new Date(timestamp)
  if (Number.isNaN(date.getTime())) return ''
  const day = `${pad(date.getFullYear(), 4)}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  return `${day} ${time}.${pad(date.getMilliseconds(), 3)}`
}

/** Lines saying what the view filtered by (none when it shows everything). */
export function filterLines(filter: ExportFilter): string[] {
  const lines: string[] = []
  if (!LEVEL_CHIPS.every((chip) => filter.levels.has(chip))) {
    const shown = LEVEL_CHIPS.filter((chip) => filter.levels.has(chip)).map((chip) => LEVEL_CHIP_LABELS[chip])
    lines.push(`Levels: ${shown.length > 0 ? shown.join(' ') : 'none'}`)
  }
  if (filter.loggers.trim() !== '') lines.push(`Loggers: ${filter.loggers.trim()}`)
  if (filter.search !== '') {
    const how = [filter.regex ? 'regex' : 'text', ...(filter.caseSensitive ? ['match case'] : [])].join(', ')
    const only = filter.onlyMatching ? ', matching entries only' : ''
    lines.push(`Search (${how}${only}): ${filter.search}`)
  }
  return lines
}

/** An entry as the viewer shows it, with the local date: `time LEVEL db logger: message`. */
function readableEntry(record: LogRecord): string {
  const event = eventOf(record)
  if (record.mark && event) {
    const icon = event.icon ? `${event.icon} ` : ''
    return `${localTime(record.mark.at)} ${event.tag.padEnd(6)} ${icon}${event.text}`
  }
  const parts: string[] = []
  const time = localTime(record.timestamp)
  if (time) parts.push(time)
  if (record.level) parts.push(LEVEL_LABELS[record.level].padEnd(6))
  if (record.db) parts.push(record.db)
  const head = parts.join(' ')
  const logger = record.logger ? `${record.logger}: ` : ''
  return head ? `${head} ${logger}${record.message}` : `${logger}${record.message}`
}

function readableMarker(row: Exclude<LogRow, { kind: 'entry' }>): string {
  if (row.kind === 'reset') return '── Log started over ──'
  const what = row.skipped === 1 ? 'entry' : 'entries'
  return `⋯ ${row.skipped.toLocaleString('en')} ${what} dropped from memory (still in the log file)`
}

function entriesOf(rows: readonly LogRow[]): LogRecord[] {
  const out: LogRecord[] = []
  for (const row of rows) if (row.kind === 'entry') out.push(row.record)
  return out
}

/** Builds the export file of `input` in `format`. */
export function buildExport(input: ExportInput, format: ExportFormat): ExportFile {
  const { project, instance, rows, filter, now } = input
  const records = entriesOf(rows)
  let text: string
  if (format === 'log') {
    text = records.map(entryText).join('\n')
  } else if (format === 'txt') {
    const head = [
      `OdHive log export: ${project}/${instance}`,
      `Exported: ${localTime(now.toISOString())} (times are local)`,
      `Entries: ${records.length.toLocaleString('en')}`,
      ...filterLines(filter),
    ]
    const body = rows.map((row) => (row.kind === 'entry' ? readableEntry(row.record) : readableMarker(row)))
    text = [...head, '', ...body].join('\n')
  } else {
    const document = {
      project,
      instance,
      exported_at: now.toISOString(),
      filter: {
        levels: LEVEL_CHIPS.filter((chip) => filter.levels.has(chip)),
        loggers: filter.loggers.trim(),
        search: filter.search,
        regex: filter.regex,
        case_sensitive: filter.caseSensitive,
        only_matching: filter.onlyMatching,
      },
      entries: records,
    }
    text = JSON.stringify(document, null, 2)
  }
  if (text !== '' && !text.endsWith('\n')) text += '\n'
  return { name: exportName(project, instance, now, format), type: MIME[format], text, entries: records.length }
}

/** Hands `file` to the browser as a download. */
export function download(file: Pick<ExportFile, 'name' | 'type' | 'text'>): void {
  const url = URL.createObjectURL(new Blob([file.text], { type: file.type }))
  const link = document.createElement('a')
  link.href = url
  link.download = file.name
  link.rel = 'noopener'
  link.style.display = 'none'
  document.body.append(link)
  link.click()
  link.remove()
  // Revoked later: some browsers start reading the blob after `click` returns.
  setTimeout(() => URL.revokeObjectURL(url), 60_000)
}
