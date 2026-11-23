import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { fakeSockets, instanceView, live, project, routedFetch } from '../test-fixtures'
import type { ServerMessage } from './api'
import { ApiClient } from './api/client'
import { JobsStore } from './jobs.svelte'
import { LiveConnection } from './live.svelte'
import { ProjectsStore } from './projects.svelte'
import { connectStores, RESYNC_DELAY_MS } from './stores'

function setup() {
  const sockets = fakeSockets()
  const { fetcher, calls } = routedFetch({ 'GET /projects': [200, [project('shop', [instanceView('main')])]] })
  const stores = {
    connection: new LiveConnection({ url: 'ws://x/api/v1/ws', createSocket: sockets.create, backoff: [100] }),
    projects: new ProjectsStore(new ApiClient(fetcher)),
    jobs: new JobsStore(),
    notifier: { handle: vi.fn() },
  }
  const disconnect = connectStores(stores)
  return { sockets, calls, stores, disconnect }
}

describe('connectStores', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('loads projects and subscribes to events and jobs on open', async () => {
    const { sockets, calls } = setup()
    await vi.waitFor(() => expect(calls).toHaveLength(1))
    sockets.last().open()
    expect(sockets.last().sent).toEqual([{ op: 'subscribe_events' }, { op: 'subscribe_jobs' }])
  })

  it('feeds every message to the stores and the notifier', async () => {
    const { sockets, stores } = setup()
    await stores.projects.load()
    sockets.last().open()
    const snapshot: ServerMessage = { type: 'snapshot', instances: [live('shop', 'main', { state: 'running', pid: 1 })] }
    sockets.last().receive(snapshot)
    sockets.last().receive({ type: 'job_snapshot', jobs: [] })
    expect(stores.projects.find('shop')?.instances[0]?.live?.status.state).toBe('running')
    expect(stores.notifier.handle).toHaveBeenCalledWith(snapshot)
    expect(stores.notifier.handle).toHaveBeenCalledTimes(2)
  })

  it('reloads projects and subscribes again after a reconnect', async () => {
    const { sockets, calls } = setup()
    sockets.last().open()
    await vi.waitFor(() => expect(calls).toHaveLength(1))
    sockets.last().drop()
    vi.advanceTimersByTime(100)
    sockets.last().open()
    expect(sockets.last().sent).toEqual([{ op: 'subscribe_events' }, { op: 'subscribe_jobs' }])
    await vi.waitFor(() => expect(calls).toHaveLength(2))
  })

  it('asks once for a snapshot after a burst of stale events', async () => {
    const { sockets, stores } = setup()
    await stores.projects.load()
    const socket = sockets.last()
    socket.open()
    socket.sent = []
    for (const instance of ['a', 'b', 'c']) {
      socket.receive({ type: 'events', events: [{ project: 'shop', instance, event: 'status', status: { state: 'running', pid: 1 } }] })
    }
    vi.advanceTimersByTime(RESYNC_DELAY_MS)
    expect(socket.sent).toEqual([{ op: 'subscribe_events' }])
  })

  it('disconnects for good', () => {
    const { sockets, stores, disconnect } = setup()
    sockets.last().open()
    disconnect()
    expect(stores.connection.status).toBe('stopped')
    expect(sockets.last().closed).toBe(1000)
  })
})
