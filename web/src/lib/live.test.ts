import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { fakeSockets } from '../test-fixtures'
import type { ServerMessage } from './api'
import { LiveConnection, socketUrl } from './live.svelte'

function setup(backoff = [100, 200, 400]) {
  const sockets = fakeSockets()
  const connection = new LiveConnection({ url: 'ws://localhost:7069/api/v1/ws', createSocket: sockets.create, backoff })
  return { sockets, connection }
}

describe('LiveConnection', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('builds the WebSocket URL from the page location', () => {
    expect(socketUrl({ protocol: 'http:', host: 'localhost:7069' })).toBe('ws://localhost:7069/api/v1/ws')
    expect(socketUrl({ protocol: 'https:', host: '[::1]:7069' })).toBe('wss://[::1]:7069/api/v1/ws')
  })

  it('connects, delivers parsed messages and ignores junk', () => {
    const { sockets, connection } = setup()
    const received: ServerMessage[] = []
    connection.onmessage((message) => received.push(message))
    connection.start()
    expect(connection.status).toBe('connecting')
    sockets.last().open()
    expect(connection.status).toBe('open')

    sockets.last().receive({ type: 'hello', version: '0.1.0' })
    sockets.last().receive('not json')
    sockets.last().receive('{"no":"type"}')
    sockets.last().receive('[1,2]')
    expect(received).toEqual([{ type: 'hello', version: '0.1.0' }])
    expect(connection.version).toBe('0.1.0')
  })

  it('sends only while open', () => {
    const { sockets, connection } = setup()
    connection.start()
    expect(connection.send({ op: 'subscribe_events' })).toBe(false)
    sockets.last().open()
    expect(connection.send({ op: 'subscribe_events' })).toBe(true)
    expect(sockets.last().sent).toEqual([{ op: 'subscribe_events' }])
  })

  it('reconnects with growing delays and runs open listeners again', () => {
    const { sockets, connection } = setup()
    const opened = vi.fn()
    connection.onopen(opened)
    connection.start()
    sockets.last().open()
    expect(opened).toHaveBeenCalledTimes(1)

    sockets.last().drop()
    expect(connection.status).toBe('reconnecting')
    vi.advanceTimersByTime(99)
    expect(sockets.sockets).toHaveLength(1)
    vi.advanceTimersByTime(1)
    expect(sockets.sockets).toHaveLength(2)

    // Fails again: the next wait is longer, and the last one repeats.
    sockets.last().drop()
    vi.advanceTimersByTime(199)
    expect(sockets.sockets).toHaveLength(2)
    vi.advanceTimersByTime(1)
    expect(sockets.sockets).toHaveLength(3)
    sockets.last().drop()
    vi.advanceTimersByTime(400)
    sockets.last().drop()
    vi.advanceTimersByTime(400)
    expect(sockets.sockets).toHaveLength(5)
    expect(connection.failures).toBe(4)

    sockets.last().open()
    expect(connection.status).toBe('open')
    expect(connection.failures).toBe(0)
    expect(opened).toHaveBeenCalledTimes(2)

    // After a good connection the backoff starts over.
    sockets.last().drop()
    vi.advanceTimersByTime(100)
    expect(sockets.sockets).toHaveLength(6)
  })

  it('also reconnects when the server closes (shutdown, slow client)', () => {
    const { sockets, connection } = setup()
    connection.start()
    sockets.last().open()
    sockets.last().drop(1001)
    vi.advanceTimersByTime(100)
    expect(sockets.sockets).toHaveLength(2)
  })

  it('retries at once on request while waiting', () => {
    const { sockets, connection } = setup([10_000])
    connection.start()
    connection.retryNow() // not waiting yet: no-op
    expect(sockets.sockets).toHaveLength(1)
    sockets.last().drop()
    connection.retryNow()
    expect(sockets.sockets).toHaveLength(2)
    vi.advanceTimersByTime(10_000)
    expect(sockets.sockets).toHaveLength(2)
  })

  it('stops for good and ignores the old socket', () => {
    const { sockets, connection } = setup()
    const received = vi.fn()
    connection.onmessage(received)
    connection.start()
    const socket = sockets.last()
    socket.open()
    connection.stop()
    expect(connection.status).toBe('stopped')
    expect(socket.closed).toBe(1000)
    socket.receive({ type: 'hello', version: 'x' })
    socket.drop()
    vi.advanceTimersByTime(10_000)
    expect(received).not.toHaveBeenCalled()
    expect(sockets.sockets).toHaveLength(1)
  })

  it('treats a socket that cannot be created like a failed connection', () => {
    let calls = 0
    const sockets = fakeSockets()
    const connection = new LiveConnection({
      url: 'ws://x',
      backoff: [50],
      createSocket: (url) => {
        calls += 1
        if (calls === 1) throw new DOMException('blocked', 'SecurityError')
        return sockets.create(url)
      },
    })
    connection.start()
    expect(connection.status).toBe('reconnecting')
    vi.advanceTimersByTime(50)
    expect(sockets.sockets).toHaveLength(1)
  })

  it('unsubscribes listeners', () => {
    const { sockets, connection } = setup()
    const received = vi.fn()
    const off = connection.onmessage(received)
    connection.start()
    sockets.last().open()
    off()
    sockets.last().receive({ type: 'hello', version: 'x' })
    expect(received).not.toHaveBeenCalled()
  })
})
