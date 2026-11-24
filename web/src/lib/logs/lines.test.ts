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
})
