import { describe, expect, it } from 'vitest'
import { StreamPool } from './pool'
import { FakeConnection, msg, records } from './test-fixtures'

const settle = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0))

describe('StreamPool', () => {
  it('shares one stream (and one subscription) per instance', () => {
    const connection = new FakeConnection()
    const pool = new StreamPool(connection)
    const a = pool.acquire('shop', 'main')
    const b = pool.acquire('shop', 'main')
    const other = pool.acquire('shop', 'test')
    expect(a).toBe(b)
    expect(other).not.toBe(a)
    expect(pool.size).toBe(2)
    expect(connection.sent.map((m) => 'instance' in m && m.instance)).toEqual(['main', 'test'])
  })

  it('stops a stream only when its last user releases it', async () => {
    const connection = new FakeConnection()
    const pool = new StreamPool(connection)
    const a = pool.acquire('shop', 'main')
    pool.acquire('shop', 'main')
    pool.release(a)
    await settle()
    expect(pool.size).toBe(1)
    expect(connection.sent).toHaveLength(1)
    pool.release(a)
    await settle()
    expect(pool.size).toBe(0)
    expect(connection.sent.at(-1)).toEqual({ op: 'unsubscribe_logs', project: 'shop', instance: 'main' })
    expect(connection.listeners).toBe(0)
  })

  it('keeps the rows when a view is replaced by another of the same instance', async () => {
    const connection = new FakeConnection()
    const pool = new StreamPool(connection)
    const first = pool.acquire('shop', 'main')
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 3)))
    pool.release(first)
    const second = pool.acquire('shop', 'main')
    await settle()
    expect(second).toBe(first)
    expect(second.rows).toHaveLength(3)
    expect(connection.sent).toHaveLength(1)
  })

  it('starts over after a stream was stopped', async () => {
    const connection = new FakeConnection()
    const pool = new StreamPool(connection)
    const first = pool.acquire('shop', 'main')
    pool.release(first)
    await settle()
    const second = pool.acquire('shop', 'main')
    expect(second).not.toBe(first)
    expect(connection.sent.at(-1)).toMatchObject({ op: 'subscribe_logs', instance: 'main' })
  })

  it('ignores extra and foreign releases', async () => {
    const connection = new FakeConnection()
    const pool = new StreamPool(connection)
    const other = new StreamPool(connection).acquire('shop', 'main')
    const mine = pool.acquire('shop', 'main')
    pool.release(other)
    await settle()
    expect(pool.size).toBe(1)
    pool.release(mine)
    pool.release(mine)
    await settle()
    expect(pool.size).toBe(0)
    // A stale release of the old stream must not end a new one.
    const again = pool.acquire('shop', 'main')
    pool.release(mine)
    await settle()
    expect(pool.size).toBe(1)
    expect(pool.acquire('shop', 'main')).toBe(again)
  })
})
