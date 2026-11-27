import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PostgresLog } from '../api'
import { PostgresLogStream } from './postgres.svelte'
import type { LogRow } from './rows'
import { record, records } from './test-fixtures'

function log(extra: Partial<PostgresLog> = {}): PostgresLog {
  return {
    source: 'file',
    unit: null,
    path: '/var/log/postgresql/postgresql-16-main.log',
    configured: null,
    found: ['/var/log/postgresql/postgresql-16-main.log'],
    status: 'ok',
    message: null,
    hint: null,
    stream: 1,
    records: [],
    skipped: 0,
    next: 1,
    ...extra,
  }
}

const seqs = (rows: LogRow[]): (number | string)[] =>
  rows.map((row) => (row.kind === 'entry' ? row.record.seq : row.kind))

function setup(answers: PostgresLog[], hidden = () => false) {
  const queries: { from?: number; limit?: number }[] = []
  const fetch = vi.fn((query: { from?: number; limit?: number }) => {
    queries.push(query)
    const answer = answers.shift()
    return answer ? Promise.resolve(answer) : Promise.reject(new Error('offline'))
  })
  const stream = new PostgresLogStream(fetch, { interval: 1000, tail: 100, hidden })
  return { stream, queries, fetch }
}

describe('PostgresLogStream', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('reads the newest entries, then polls from next', async () => {
    const { stream, queries } = setup([
      log({ records: records(1, 2), next: 3 }),
      log({ records: [record(3, 'a\nb')], next: 4 }),
    ])
    stream.start()
    await vi.advanceTimersByTimeAsync(0)
    expect(stream.running).toBe(true)
    expect(seqs(stream.rows)).toEqual([1, 2])
    await vi.advanceTimersByTimeAsync(1000)
    expect(queries).toEqual([{ limit: 100 }, { from: 3 }])
    expect(seqs(stream.rows)).toEqual([1, 2, 3])
    expect(stream.rows[2]?.lines).toBe(2)
    expect(stream.rows.map((r) => r.id)).toEqual([1, 2, 3])
    stream.stop()
  })

  it('starts over when the server read the log anew', async () => {
    const { stream, queries } = setup([
      log({ records: records(1, 2), next: 3 }),
      log({ stream: 2, records: records(3, 3), next: 4 }),
      log({ stream: 2, records: records(1, 1), next: 2 }),
    ])
    stream.start()
    await vi.advanceTimersByTimeAsync(0)
    await vi.advanceTimersByTimeAsync(1000)
    await vi.advanceTimersByTimeAsync(10)
    expect(queries).toEqual([{ limit: 100 }, { from: 3 }, { limit: 100 }])
    expect(seqs(stream.rows)).toEqual([1])
    expect(stream.firstId).toBe(stream.rows[0]?.id)
    stream.stop()
  })

  it('marks gaps', async () => {
    const { stream } = setup([log({ records: records(1, 1), next: 2 }), log({ skipped: 5, records: records(7, 7), next: 8 })])
    stream.start()
    await vi.advanceTimersByTimeAsync(1000)
    expect(seqs(stream.rows)).toEqual([1, 'gap', 7])
    stream.stop()
  })

  it('reports a log it cannot read and clears the rows', async () => {
    const { stream } = setup([
      log({ records: records(1, 1), next: 2 }),
      log({ status: 'permission_denied', path: null, hint: 'join adm', message: 'denied' }),
    ])
    stream.start()
    await vi.advanceTimersByTimeAsync(1000)
    expect(stream.running).toBe(false)
    expect(stream.info?.hint).toBe('join adm')
    expect(stream.rows).toEqual([])
    stream.stop()
  })

  it('keeps the rows and says why when a read fails', async () => {
    const { stream } = setup([log({ records: records(1, 1), next: 2 })])
    stream.start()
    await vi.advanceTimersByTimeAsync(1000)
    expect(stream.error).toBe('offline')
    expect(seqs(stream.rows)).toEqual([1])
    stream.stop()
  })

  it('does not read while the page is hidden, and stops polling on stop', async () => {
    let hidden = true
    const { stream, fetch } = setup([log(), log()], () => hidden)
    stream.start()
    await vi.advanceTimersByTimeAsync(3000)
    expect(fetch).not.toHaveBeenCalled()
    hidden = false
    await vi.advanceTimersByTimeAsync(1000)
    expect(fetch).toHaveBeenCalledTimes(1)
    stream.stop()
    await vi.advanceTimersByTimeAsync(5000)
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('restart drops the rows and an answer still on its way', async () => {
    let release: (log: PostgresLog) => void = () => {}
    const fetch = vi.fn(
      (query: { from?: number; limit?: number }) =>
        new Promise<PostgresLog>((resolve) => {
          release = resolve
          void query
        }),
    )
    const stream = new PostgresLogStream(fetch, { interval: 1000, hidden: () => false })
    stream.accept(log({ records: records(1, 2), next: 3 }))
    expect(seqs(stream.rows)).toEqual([1, 2])
    stream.start()
    const stale = release
    stream.restart()
    expect(stream.rows).toEqual([])
    stale(log({ records: records(9, 9), next: 10 }))
    await vi.advanceTimersByTimeAsync(0)
    expect(stream.rows).toEqual([])
    release(log({ path: '/other.log', records: records(1, 1), next: 2 }))
    await vi.advanceTimersByTimeAsync(0)
    expect(seqs(stream.rows)).toEqual([1])
    stream.stop()
  })
})
