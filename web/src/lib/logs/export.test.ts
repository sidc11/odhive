import { afterEach, describe, expect, it, vi } from 'vitest'
import { buildExport, download, exportName, filterLines, localTime, type ExportFilter, type ExportInput } from './export'
import { LEVEL_CHIPS, type LevelChip } from './filter'
import type { LogRow } from './rows'
import { record } from './test-fixtures'

const entry = (id: number, message = `entry ${id}`, extra = {}): LogRow => ({
  id,
  kind: 'entry',
  record: record(id, message, extra),
  lines: message.split('\n').length,
})

const ALL = new Set<LevelChip>(LEVEL_CHIPS)
const NO_FILTER: ExportFilter = {
  levels: ALL,
  loggers: '',
  search: '',
  regex: false,
  caseSensitive: false,
  onlyMatching: false,
}
// Local 2026-11-25 14:03:09.
const NOW = new Date(2026, 10, 25, 14, 3, 9, 5)

function input(rows: LogRow[], filter: Partial<ExportFilter> = {}): ExportInput {
  return { project: 'shop', instance: 'main', rows, filter: { ...NO_FILTER, ...filter }, now: NOW }
}

const ROWS: LogRow[] = [
  entry(1, 'Modules loaded.', { db: 'shop', pid: 77 }),
  { id: 2, kind: 'gap', skipped: 1200, lines: 1 },
  entry(3, 'boom\nTraceback (most recent call last):\n  x', { level: 'error' }),
  { id: 4, kind: 'reset', lines: 1 },
  entry(5, 'ImportError: no odoo', { timestamp: null, level: null, logger: null, pid: null }),
]

describe('exportName', () => {
  it('names the file after the instance and local time', () => {
    expect(exportName('shop', 'main', NOW, 'log')).toBe('shop-main-20261125-140309.log')
  })

  it('keeps path separators and dot files out of the name', () => {
    expect(exportName('../etc', 'a b/c', NOW, 'json')).toBe('_etc-a_b_c-20261125-140309.json')
    expect(exportName('', '...', NOW, 'txt')).toBe('log-log-20261125-140309.txt')
  })
})

describe('localTime', () => {
  it('formats a local date and time, empty for none', () => {
    expect(localTime(new Date(2026, 0, 2, 3, 4, 5, 6).toISOString())).toBe('2026-01-02 03:04:05.006')
    expect(localTime(null)).toBe('')
    expect(localTime('nope')).toBe('')
  })
})

describe('filterLines', () => {
  it('is empty when everything is shown', () => {
    expect(filterLines(NO_FILTER)).toEqual([])
  })

  it('describes levels, loggers and the search', () => {
    expect(
      filterLines({
        levels: new Set<LevelChip>(['warning', 'error']),
        loggers: ' odoo.addons -werkzeug ',
        search: 'sale\\.order',
        regex: true,
        caseSensitive: true,
        onlyMatching: true,
      }),
    ).toEqual([
      'Levels: WARN ERROR',
      'Loggers: odoo.addons -werkzeug',
      'Search (regex, match case, matching entries only): sale\\.order',
    ])
    expect(filterLines({ ...NO_FILTER, levels: new Set(), search: 'x' })).toEqual([
      'Levels: none',
      'Search (text): x',
    ])
  })
})

describe('buildExport', () => {
  it('writes .log as the Odoo log lines, markers left out', () => {
    const file = buildExport(input(ROWS), 'log')
    expect(file).toEqual({
      name: 'shop-main-20261125-140309.log',
      type: 'text/plain;charset=utf-8',
      entries: 3,
      text:
        '2026-11-24 09:00:00,000 77 INFO shop odoo.modules.loading: Modules loaded.\n' +
        '2026-11-24 09:00:00,000 4242 ERROR ? odoo.modules.loading: boom\nTraceback (most recent call last):\n  x\n' +
        'ImportError: no odoo\n',
    })
  })

  it('writes .txt with a header, local times and the markers', () => {
    const file = buildExport(input(ROWS, { loggers: 'loading' }), 'txt')
    const time = localTime('2026-11-24T09:00:00.000Z')
    expect(file.text.split('\n')).toEqual([
      'OdHive log export: shop/main',
      'Exported: 2026-11-25 14:03:09.005 (times are local)',
      'Entries: 3',
      'Loggers: loading',
      '',
      `${time} INFO   shop odoo.modules.loading: Modules loaded.`,
      '⋯ 1,200 entries dropped from memory (still in the log file)',
      `${time} ERROR  odoo.modules.loading: boom`,
      'Traceback (most recent call last):',
      '  x',
      '── Log started over ──',
      'ImportError: no odoo',
      '',
    ])
  })

  it('writes .json with the records and the filter', () => {
    const file = buildExport(input(ROWS, { levels: new Set(['error']), search: 'boom', onlyMatching: true }), 'json')
    expect(file.type).toBe('application/json;charset=utf-8')
    const parsed = JSON.parse(file.text) as Record<string, unknown>
    expect(parsed).toEqual({
      project: 'shop',
      instance: 'main',
      exported_at: NOW.toISOString(),
      filter: {
        levels: ['error'],
        loggers: '',
        search: 'boom',
        regex: false,
        case_sensitive: false,
        only_matching: true,
      },
      entries: ROWS.flatMap((row) => (row.kind === 'entry' ? [row.record] : [])),
    })
  })

  it('keeps markup and control text as data', () => {
    const xss = '<script>alert(1)</script>"\u0007'
    const rows = [entry(1, xss)]
    expect(buildExport(input(rows), 'log').text).toContain(xss)
    const parsed = JSON.parse(buildExport(input(rows), 'json').text) as { entries: { message: string }[] }
    expect(parsed.entries[0]?.message).toBe(xss)
  })

  it('writes an empty .log for no entries', () => {
    expect(buildExport(input([]), 'log')).toMatchObject({ text: '', entries: 0 })
  })
})

describe('download', () => {
  afterEach(() => {
    vi.restoreAllMocks()
    vi.useRealTimers()
  })

  it('clicks a temporary download link and revokes its URL later', () => {
    vi.useFakeTimers()
    const create = vi.fn<(blob: Blob) => string>(() => 'blob:odhive/1')
    const revoke = vi.fn()
    Object.assign(URL, { createObjectURL: create, revokeObjectURL: revoke })
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => undefined)

    download({ name: 'shop-main.log', type: 'text/plain', text: 'x\n' })

    expect(create).toHaveBeenCalledOnce()
    expect(create.mock.calls[0]?.[0].type).toBe('text/plain')
    expect(click).toHaveBeenCalledOnce()
    const link = click.mock.contexts[0] as HTMLAnchorElement | undefined
    expect(link?.download).toBe('shop-main.log')
    expect(link?.getAttribute('href')).toBe('blob:odhive/1')
    expect(link?.isConnected).toBe(false)
    expect(revoke).not.toHaveBeenCalled()
    vi.advanceTimersByTime(60_000)
    expect(revoke).toHaveBeenCalledWith('blob:odhive/1')
  })
})
