import { afterEach, describe, expect, it } from 'vitest'
import { entryCount, entryText, odooTime, rowsText, selectedRowIds } from './copy'
import type { LogRow } from './rows'
import { record } from './test-fixtures'

const entry = (id: number, message = `entry ${id}`): LogRow => ({
  id,
  kind: 'entry',
  record: record(id, message),
  lines: message.split('\n').length,
})

describe('odooTime', () => {
  it("formats Odoo's UTC asctime", () => {
    expect(odooTime('2026-11-24T09:05:07.042Z')).toBe('2026-11-24 09:05:07,042')
    expect(odooTime('2026-01-02T03:04:05.006+02:00')).toBe('2026-01-02 01:04:05,006')
    expect(odooTime('not a time')).toBeNull()
  })
})

describe('entryText', () => {
  it('rebuilds the log line of an entry', () => {
    expect(entryText(record(1, 'Modules loaded.', { db: 'shop', pid: 77, level: 'warning' }))).toBe(
      '2026-11-24 09:00:00,000 77 WARNING shop odoo.modules.loading: Modules loaded.',
    )
    expect(entryText(record(2, 'x', { level: 'runbot' }))).toBe(
      '2026-11-24 09:00:00,000 4242 RUNBOT ? odoo.modules.loading: x',
    )
  })

  it('keeps continuation lines and header-less entries as they are', () => {
    expect(entryText(record(1, 'boom\nTraceback (most recent call last):\n  x'))).toBe(
      '2026-11-24 09:00:00,000 4242 INFO ? odoo.modules.loading: boom\nTraceback (most recent call last):\n  x',
    )
    const bare = record(2, 'ImportError: no odoo', {
      timestamp: null,
      level: null,
      logger: null,
      pid: null,
    })
    expect(entryText(bare)).toBe('ImportError: no odoo')
  })

  it('renders markup as the text it is', () => {
    expect(entryText(record(1, '<script>alert(1)</script>', { timestamp: null }))).toBe('<script>alert(1)</script>')
  })
})

describe('rowsText', () => {
  it('joins entries one per line and leaves markers out', () => {
    const rows: LogRow[] = [
      entry(1, 'a\nb'),
      { id: 2, kind: 'gap', skipped: 3, lines: 1 },
      entry(3),
      { id: 4, kind: 'reset', lines: 1 },
    ]
    const text = rowsText(rows)
    expect(text.split('\n')).toHaveLength(3)
    expect(text).toMatch(/: a\nb\n.*: entry 3$/)
    expect(entryCount(rows)).toBe(2)
    expect(rowsText([])).toBe('')
  })
})

describe('selectedRowIds', () => {
  afterEach(() => {
    document.getSelection()?.removeAllRanges()
    document.body.replaceChildren()
  })

  function list(): HTMLElement {
    const container = document.createElement('div')
    for (const id of [5, 6, 7, 8]) {
      const row = document.createElement('div')
      row.dataset['rowId'] = String(id)
      row.append(document.createTextNode(`row ${id}`))
      container.append(row)
    }
    document.body.append(container)
    return container
  }

  function select(start: Node, startOffset: number, end: Node, endOffset: number): Selection {
    const selection = document.getSelection()
    if (!selection) throw new Error('no selection')
    const range = document.createRange()
    range.setStart(start, startOffset)
    range.setEnd(end, endOffset)
    selection.removeAllRanges()
    selection.addRange(range)
    return selection
  }

  const text = (container: HTMLElement, id: number): Node =>
    container.querySelector(`[data-row-id="${id}"]`)?.firstChild ?? container

  it('gives the rows a selection touches', () => {
    const container = list()
    expect(selectedRowIds(select(text(container, 6), 2, text(container, 8), 1), container)).toEqual({ from: 6, to: 8 })
    expect(selectedRowIds(select(text(container, 7), 0, text(container, 7), 3), container)).toEqual({ from: 7, to: 7 })
  })

  it('finds rows when an end is on the list itself', () => {
    const container = list()
    expect(selectedRowIds(select(text(container, 6), 1, container, 4), container)).toEqual({ from: 6, to: 8 })
  })

  it('ignores empty selections and text outside the log', () => {
    const container = list()
    expect(selectedRowIds(null, container)).toBeNull()
    expect(selectedRowIds(select(text(container, 6), 2, text(container, 6), 2), container)).toBeNull()
    const outside = document.createElement('p')
    outside.textContent = 'elsewhere'
    document.body.append(outside)
    const other = outside.firstChild ?? outside
    expect(selectedRowIds(select(other, 0, other, 4), container)).toBeNull()
    expect(selectedRowIds(select(other, 0, text(container, 6), 2), container)).toBeNull()
    expect(selectedRowIds(select(text(container, 6), 0, text(container, 7), 2), undefined)).toBeNull()
  })
})
