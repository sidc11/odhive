import { describe, expect, it } from 'vitest'
import { LogLines } from './lines'
import { LogStream } from './stream.svelte'
import { FakeConnection, msg, record, records } from './test-fixtures'

function setup(maxRows = 1000) {
  const connection = new FakeConnection()
  const stream = new LogStream(connection, 'shop', 'main', { maxRows })
  stream.start()
  connection.receive(msg.subscribed(1))
  return { connection, stream, lines: new LogLines(stream) }
}

describe('LogLines', () => {
  it('follows the stream incrementally', () => {
    const { connection, lines } = setup()
    expect(lines.sync()).toEqual({ added: 0, removedLines: 0 })
    connection.receive(msg.logs([record(1), record(2, 'a\nb')]))
    expect(lines.sync()).toEqual({ added: 2, removedLines: 0 })
    expect(lines.layout.total).toBe(3)
    connection.receive(msg.logs(records(3, 4)))
    expect(lines.sync()).toEqual({ added: 2, removedLines: 0 })
    expect(lines.items.map((row) => row.id)).toEqual([1, 2, 3, 4])
    expect(lines.sync()).toEqual({ added: 0, removedLines: 0 })
  })

  it('drops what the stream dropped and reports its lines', () => {
    const { connection, lines } = setup(10)
    connection.receive(msg.logs([record(1, 'a\nb\nc'), ...records(2, 10)]))
    lines.sync()
    connection.receive(msg.logs(records(11, 11)))
    // 11 rows > 10: the stream drops 2 (1 over + a tenth).
    expect(lines.sync()).toEqual({ added: 1, removedLines: 4 })
    expect(lines.items[0]?.id).toBe(3)
    expect(lines.layout.length).toBe(9)
    expect(lines.layout.total).toBe(9)
  })

  it('shows only rows passing its filter', () => {
    const { connection, lines } = setup()
    connection.receive(msg.logs([record(1), record(2, 'x', { level: 'error' }), record(3)]))
    lines.setFilter((row) => row.kind === 'entry' && row.record.level === 'error')
    expect(lines.items.map((row) => row.id)).toEqual([2])
    connection.receive(msg.logs([record(4, 'y', { level: 'error' }), record(5)]))
    expect(lines.sync().added).toBe(1)
    expect(lines.items.map((row) => row.id)).toEqual([2, 4])
  })

  it('tracks search matches as rows arrive and leave', () => {
    const { connection, lines } = setup(10)
    connection.receive(msg.logs([record(1, 'hit'), record(2), record(3, 'hit')]))
    lines.sync()
    lines.setSearch((row) => row.kind === 'entry' && row.record.message === 'hit')
    expect(lines.matches).toEqual([1, 3])
    connection.receive(msg.logs([record(4, 'hit'), ...records(5, 10)]))
    lines.sync()
    expect(lines.matches).toEqual([1, 3, 4])
    // 11 rows: the stream drops rows 1 and 2.
    connection.receive(msg.logs(records(11, 11)))
    lines.sync()
    expect(lines.matches).toEqual([3, 4])
    lines.setFilter((row) => row.id !== 4)
    expect(lines.matches).toEqual([3])
    lines.setSearch(null)
    expect(lines.matches).toEqual([])
  })

  it('finds the next and previous match, wrapping around', () => {
    const { connection, lines } = setup()
    connection.receive(msg.logs(records(1, 10)))
    lines.sync()
    expect(lines.nextMatch(1, true)).toBeNull()
    lines.setSearch((row) => [3, 6, 9].includes(row.id))
    expect(lines.nextMatch(3, true)).toBe(6)
    expect(lines.nextMatch(3, false)).toBe(9)
    expect(lines.nextMatch(9, true)).toBe(3)
    expect(lines.nextMatch(4, false)).toBe(3)
    expect(lines.nextMatch(6, true, true)).toBe(6)
    expect(lines.nextMatch(6, false, true)).toBe(6)
    expect(lines.nextMatch(7, false, true)).toBe(6)
  })

  it('finds rows by id', () => {
    const { connection, lines } = setup()
    connection.receive(msg.logs(records(1, 6)))
    lines.setFilter((row) => row.id % 2 === 0)
    expect(lines.indexOfId(4)).toBe(1)
    expect(lines.indexOfId(5)).toBe(2)
    expect(lines.indexOfId(99)).toBe(3)
  })

  it('gives the shown rows between two ids', () => {
    const { connection, lines } = setup()
    connection.receive(msg.logs(records(1, 6)))
    lines.setFilter((row) => row.id !== 3)
    expect(lines.between(2, 5).map((row) => row.id)).toEqual([2, 4, 5])
    expect(lines.between(3, 3)).toEqual([])
    expect(lines.between(5, 99).map((row) => row.id)).toEqual([5, 6])
  })

  it('folds tracebacks until unfolded, across filters and drops', () => {
    const { connection, lines } = setup(10)
    const trace = record(2, 'boom\nTraceback (most recent call last):\nValueError: x', { exceptions: ['ValueError: x'] })
    connection.receive(msg.logs([record(1), trace, record(3, 'a\nb')]))
    lines.sync()
    // Folded: one line; plain multi-line rows are never folded.
    expect(lines.layout.total).toBe(4)
    expect(lines.isExpanded(2)).toBe(false)
    expect(lines.setExpanded(2, true)).toBe(2)
    expect(lines.setExpanded(2, true)).toBe(0)
    expect(lines.setExpanded(3, true)).toBe(0)
    expect(lines.setExpanded(99, true)).toBe(0)
    expect(lines.layout.total).toBe(6)
    expect(lines.layout.offsetOf(2)).toBe(4)
    // Kept when the filter rebuilds the view.
    lines.setFilter(() => true)
    expect(lines.layout.total).toBe(6)
    expect(lines.setExpanded(2, false)).toBe(-2)
    expect(lines.layout.total).toBe(4)
    lines.setExpanded(2, true)
    // Forgotten once the stream drops the row.
    connection.receive(msg.logs(records(4, 12)))
    lines.sync()
    expect(lines.items[0]?.id).toBeGreaterThan(2)
    expect(lines.isExpanded(2)).toBe(false)
  })
})
