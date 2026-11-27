import { describe, expect, it } from 'vitest'
import {
  chipOf,
  compileSearch,
  highlight,
  LEVEL_CHIPS,
  parseLoggers,
  rowFilter,
  rowMatcher,
  sourceOf,
  type LevelChip,
  type SourceChip,
} from './filter'
import type { LogRow } from './rows'
import { marked, record } from './test-fixtures'

function entry(id: number, extra: Parameters<typeof record>[2] = {}, message = `entry ${id}`): LogRow {
  return { id, kind: 'entry', record: record(id, message, extra), lines: 1 }
}

const ALL = new Set<LevelChip>(LEVEL_CHIPS)
const NONE = parseLoggers('')

function pattern(query: string, regex = false, caseSensitive = false): RegExp {
  const search = compileSearch(query, { regex, caseSensitive })
  if (!search.ok || !search.pattern) throw new Error('no pattern')
  return search.pattern
}

describe('level filter', () => {
  it('puts RUNBOT and header-less lines with INFO', () => {
    expect(chipOf('runbot')).toBe('info')
    expect(chipOf(null)).toBe('info')
    expect(chipOf('critical')).toBe('critical')
  })

  it('hides levels that are off and keeps markers', () => {
    const filter = rowFilter(new Set<LevelChip>(['error', 'critical']), NONE)
    expect(filter(entry(1, { level: 'error' }))).toBe(true)
    expect(filter(entry(2, { level: 'critical' }))).toBe(true)
    expect(filter(entry(3, { level: 'warning' }))).toBe(false)
    expect(filter(entry(4, { level: 'runbot' }))).toBe(false)
    expect(filter({ id: 5, kind: 'gap', skipped: 3, lines: 1 })).toBe(true)
    expect(filter({ id: 6, kind: 'reset', lines: 1 })).toBe(true)
  })
})

describe('source filter', () => {
  const sql = (id: number): LogRow => entry(id, { level: 'debug', logger: 'odoo.sql_db' }, '[0.412 ms] query: SELECT 1')

  it('puts DEBUG entries of odoo.sql_db in the SQL source, everything else in Odoo', () => {
    expect(sourceOf({ level: 'debug', logger: 'odoo.sql_db' })).toBe('sql')
    expect(sourceOf({ level: 'info', logger: 'odoo.sql_db' })).toBe('odoo')
    expect(sourceOf({ level: 'debug', logger: 'odoo.sql_db.extra' })).toBe('odoo')
    expect(sourceOf({ level: null, logger: null })).toBe('odoo')
  })

  it('hides sources that are off', () => {
    const onlyOdoo = rowFilter(ALL, NONE, new Set<SourceChip>(['odoo']))
    expect(onlyOdoo(sql(1))).toBe(false)
    expect(onlyOdoo(entry(2))).toBe(true)
    const onlySql = rowFilter(ALL, NONE, new Set<SourceChip>(['sql']))
    expect(onlySql(sql(1))).toBe(true)
    expect(onlySql(entry(2))).toBe(false)
    expect(onlySql({ id: 3, kind: 'gap', skipped: 3, lines: 1 })).toBe(true)
  })

  it('shows SQL queries whatever the level chips; the logger filter still applies', () => {
    expect(rowFilter(new Set<LevelChip>(['error']), NONE)(sql(1))).toBe(true)
    expect(rowFilter(ALL, parseLoggers('-sql_db'))(sql(1))).toBe(false)
  })
})

describe("OdHive's events", () => {
  const event = (id: number, mark: Parameters<typeof marked>[1], message?: string): LogRow => ({
    id,
    kind: 'entry',
    record: marked(id, mark, message),
    lines: 1,
  })

  it('are shown whatever the level and logger filters', () => {
    const filter = rowFilter(new Set<LevelChip>(['error']), parseLoggers('addons.sale'))
    expect(filter(event(1, { kind: 'reload', files: ['m/a.py'], file_count: 1, update: [] }))).toBe(true)
    expect(filter(event(2, { kind: 'installed', package: 'Pillow' }))).toBe(true)
    expect(filter(event(3, { kind: 'started', pid: 1 }))).toBe(true)
    // The installer's output goes with INFO and has no logger.
    expect(filter(event(4, { kind: 'install_output' }))).toBe(false)
    expect(rowFilter(ALL, NONE)(event(4, { kind: 'install_output' }))).toBe(true)
  })

  it('are searched in the text they show', () => {
    const row = event(1, { kind: 'installed', package: 'Pillow' }, 'odhive: installed Pillow; restarting')
    expect(rowMatcher(pattern('Pillow, restarting'))(row)).toBe(true)
    expect(rowMatcher(pattern('; restarting'))(row)).toBe(false)
  })
})

describe('logger filter', () => {
  it('parses include and exclude words', () => {
    expect(parseLoggers(' odoo.addons.Sale, -werkzeug  -  http ')).toEqual({
      include: ['odoo.addons.sale', 'http'],
      exclude: ['werkzeug'],
    })
  })

  it('matches parts of logger names, case-insensitively', () => {
    const filter = rowFilter(ALL, parseLoggers('addons.sale, -sale.report'))
    expect(filter(entry(1, { logger: 'odoo.addons.sale.models.order' }))).toBe(true)
    expect(filter(entry(2, { logger: 'odoo.addons.Sale.report.x' }))).toBe(false)
    expect(filter(entry(3, { logger: 'odoo.modules.loading' }))).toBe(false)
    expect(filter(entry(4, { logger: null }))).toBe(false)
  })

  it('only excludes without include words', () => {
    const filter = rowFilter(ALL, parseLoggers('-werkzeug'))
    expect(filter(entry(1, { logger: 'werkzeug' }))).toBe(false)
    expect(filter(entry(2, { logger: null }))).toBe(true)
  })
})

describe('search', () => {
  it('matches plain text literally and case-insensitively', () => {
    const match = rowMatcher(pattern('a.b('))
    expect(match(entry(1, {}, 'x A.B( y'))).toBe(true)
    expect(match(entry(2, {}, 'axb('))).toBe(false)
  })

  it('honours match case', () => {
    const match = rowMatcher(pattern('Error', false, true))
    expect(match(entry(1, {}, 'Error here'))).toBe(true)
    expect(match(entry(2, {}, 'error here'))).toBe(false)
  })

  it('runs regular expressions and reports invalid ones', () => {
    const match = rowMatcher(pattern('^Load(ing|ed) \\d+', true))
    expect(match(entry(1, {}, 'loading 12 modules'))).toBe(true)
    expect(match(entry(2, {}, 'not loading 12'))).toBe(false)
    const bad = compileSearch('(unclosed', { regex: true, caseSensitive: false })
    expect(bad.ok).toBe(false)
    if (!bad.ok) expect(bad.error).toMatch(/unterminated group/i)
    expect(compileSearch('(unclosed', { regex: false, caseSensitive: false }).ok).toBe(true)
  })

  it('searches loggers too, never markers', () => {
    const match = rowMatcher(pattern('sale.models'))
    expect(match(entry(1, { logger: 'odoo.addons.sale.models.order' }))).toBe(true)
    expect(match({ id: 2, kind: 'gap', skipped: 1, lines: 1 })).toBe(false)
  })

  it('needs a non-empty match', () => {
    const match = rowMatcher(pattern('x*', true))
    expect(match(entry(1, {}, 'abc'))).toBe(false)
    expect(match(entry(2, {}, 'abxc'))).toBe(true)
  })

  it('treats an empty search as none and refuses very long ones', () => {
    expect(compileSearch('', { regex: true, caseSensitive: false })).toEqual({ ok: true, pattern: null })
    expect(compileSearch('a'.repeat(501), { regex: false, caseSensitive: false }).ok).toBe(false)
  })
})

describe('highlight', () => {
  it('splits text into matches and the rest', () => {
    expect(highlight('foo bar foo', pattern('foo'))).toEqual([
      { text: 'foo', match: true },
      { text: ' bar ', match: false },
      { text: 'foo', match: true },
    ])
    expect(highlight('abc', null)).toEqual([{ text: 'abc', match: false }])
  })

  it('skips empty matches and caps the number of marks', () => {
    expect(highlight('abxc', pattern('x*', true))).toEqual([
      { text: 'ab', match: false },
      { text: 'x', match: true },
      { text: 'c', match: false },
    ])
    const parts = highlight('a'.repeat(5000), pattern('a'))
    expect(parts.filter((part) => part.match)).toHaveLength(1000)
    expect(parts.map((part) => part.text).join('')).toBe('a'.repeat(5000))
  })
})
