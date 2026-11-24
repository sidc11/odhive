import { describe, expect, it } from 'vitest'
import type { LogRow } from './rows'
import { LogStream } from './stream.svelte'
import { FakeConnection, msg, record, records } from './test-fixtures'

function setup(options: { tail?: number; maxRows?: number } = {}) {
  const connection = new FakeConnection()
  const stream = new LogStream(connection, 'shop', 'main', options)
  stream.start()
  return { connection, stream }
}

const seqs = (rows: LogRow[]): (number | string)[] =>
  rows.map((row) => (row.kind === 'entry' ? row.record.seq : row.kind))

describe('LogStream', () => {
  it('subscribes to the newest entries and appends what arrives', () => {
    const { connection, stream } = setup({ tail: 100 })
    expect(connection.sent).toEqual([{ op: 'subscribe_logs', project: 'shop', instance: 'main', tail: 100 }])
    connection.receive(msg.subscribed(1))
    expect(stream.running).toBe(true)
    connection.receive(msg.logs(records(1, 3)))
    connection.receive(msg.logs([record(4, 'Traceback\n  File "x.py", line 2\nKeyError: 1')]))
    expect(seqs(stream.rows)).toEqual([1, 2, 3, 4])
    expect(stream.rows.map((row) => row.id)).toEqual([1, 2, 3, 4])
    expect(stream.rows[3]?.lines).toBe(3)
    expect(stream.version).toBe(2)
  })

  it('ignores messages about other instances', () => {
    const { connection, stream } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive({ type: 'logs', project: 'shop', instance: 'other', records: records(1, 2), next: 3 })
    connection.receive({ type: 'logs', project: 'crm', instance: 'main', records: records(1, 2), next: 3 })
    connection.receive({ type: 'hello', version: '0.1.0' })
    expect(stream.rows).toEqual([])
  })

  it('waits for the connection, then subscribes on open', () => {
    const connection = new FakeConnection()
    connection.status = 'connecting'
    const stream = new LogStream(connection, 'shop', 'main')
    stream.start()
    expect(connection.sent).toEqual([])
    connection.reopen()
    expect(connection.sent).toHaveLength(1)
  })

  it('marks gaps and restarted logs', () => {
    const { connection, stream } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 2)))
    connection.receive(msg.gap(40))
    connection.receive(msg.logs(records(43, 43)))
    connection.receive(msg.reset())
    connection.receive(msg.logs(records(1, 1)))
    expect(seqs(stream.rows)).toEqual([1, 2, 'gap', 43, 'reset', 1])
    expect(stream.rows[2]).toMatchObject({ kind: 'gap', skipped: 40 })
  })

  it('does not mark a reset before the first entries', () => {
    const { connection, stream } = setup()
    connection.receive(msg.subscribed(1, false))
    connection.receive(msg.reset())
    connection.receive(msg.logs(records(1, 1)))
    expect(seqs(stream.rows)).toEqual([1])
  })

  it('resumes after a reconnect from the last entry it has', () => {
    const { connection, stream } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 5)))
    connection.status = 'reconnecting'
    connection.reopen()
    expect(connection.sent.at(-1)).toEqual({ op: 'subscribe_logs', project: 'shop', instance: 'main', from: 5 })
    connection.receive(msg.subscribed(5))
    connection.receive(msg.logs(records(5, 7)))
    expect(seqs(stream.rows)).toEqual([1, 2, 3, 4, 5, 6, 7])
  })

  it('starts over when the log changed while disconnected', () => {
    const { connection, stream } = setup({ tail: 50 })
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 5)))
    connection.reopen()
    connection.receive(msg.subscribed(5))
    // Entry 5 of a different log.
    connection.receive(msg.logs([record(5, 'something else'), record(6)]))
    expect(connection.sent.at(-1)).toEqual({ op: 'subscribe_logs', project: 'shop', instance: 'main', tail: 50 })
    // What the old cursor still sends is ignored.
    connection.receive(msg.logs(records(7, 8)))
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 2)))
    expect(seqs(stream.rows)).toEqual([1, 2, 3, 4, 5, 'reset', 1, 2])
  })

  it('starts over when the service no longer runs the instance', () => {
    const { connection, stream } = setup({ tail: 50 })
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 3)))
    connection.reopen()
    connection.receive(msg.subscribed(3, false))
    expect(stream.running).toBe(false)
    expect(connection.sent.at(-1)).toMatchObject({ tail: 50 })
    connection.receive(msg.subscribed(1, false))
    connection.receive(msg.logs(records(1, 1)))
    expect(seqs(stream.rows)).toEqual([1, 2, 3, 'reset', 1])
  })

  it('keeps at most maxRows rows: past that it drops down to 90%', () => {
    const { connection, stream } = setup({ maxRows: 100 })
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 100)))
    expect(stream.rows).toHaveLength(100)
    expect(stream.dropped).toBe(0)
    connection.receive(msg.logs(records(101, 101)))
    expect(stream.rows).toHaveLength(90)
    expect(stream.dropped).toBe(11)
    expect(stream.firstId).toBe(12)
    expect(stream.rows[0]?.id).toBe(12)
  })

  it('unsubscribes and stops listening on stop', () => {
    const { connection, stream } = setup()
    stream.stop()
    expect(connection.sent.at(-1)).toEqual({ op: 'unsubscribe_logs', project: 'shop', instance: 'main' })
    expect(connection.listeners).toBe(0)
  })
})
