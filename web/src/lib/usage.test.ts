import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { routedFetch } from '../test-fixtures'
import type { InstanceUsage } from './api'
import { ApiClient } from './api/client'
import { USAGE_POLL_MS, UsageStore } from './usage.svelte'

const MAIN: InstanceUsage = {
  project: 'shop',
  instance: 'main',
  pid: 7,
  cpu_percent: 2.5,
  memory_bytes: 200 * 1024 * 1024,
  processes: 1,
  started_at: 1000,
}

function store(routes: Parameters<typeof routedFetch>[0]) {
  const { fetcher, calls } = routedFetch(routes)
  return { usage: new UsageStore(new ApiClient(fetcher)), calls }
}

describe('UsageStore', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => {
    vi.useRealTimers()
    vi.restoreAllMocks()
  })

  it('polls while started, shared between callers', async () => {
    const { usage, calls } = store({ 'GET /instances/usage': [200, [MAIN]] })
    const stopA = usage.start()
    const stopB = usage.start()
    await vi.advanceTimersByTimeAsync(0)
    expect(calls).toHaveLength(1)
    expect(usage.get('shop', 'main')).toEqual(MAIN)
    expect(usage.get('shop', 'test')).toBeUndefined()

    await vi.advanceTimersByTimeAsync(USAGE_POLL_MS)
    expect(calls).toHaveLength(2)
    stopA()
    stopA()
    await vi.advanceTimersByTimeAsync(USAGE_POLL_MS)
    expect(calls).toHaveLength(3)
    stopB()
    await vi.advanceTimersByTimeAsync(USAGE_POLL_MS * 3)
    expect(calls).toHaveLength(3)
  })

  it('drops instances that stopped running and keeps the sample on errors', async () => {
    const routes: Parameters<typeof routedFetch>[0] = { 'GET /instances/usage': [200, [MAIN]] }
    const { usage } = store(routes)
    await usage.refresh()
    routes['GET /instances/usage'] = [500]
    await usage.refresh()
    expect(usage.get('shop', 'main')).toEqual(MAIN)
    routes['GET /instances/usage'] = [200, []]
    await usage.refresh()
    expect(usage.get('shop', 'main')).toBeUndefined()
  })

  it('skips polls while the tab is hidden', async () => {
    const { usage, calls } = store({ 'GET /instances/usage': [200, []] })
    vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden')
    await usage.refresh()
    expect(calls).toHaveLength(0)
  })

  it('ticks the clock while started', async () => {
    const { usage } = store({ 'GET /instances/usage': [200, []] })
    const stop = usage.start()
    await vi.advanceTimersByTimeAsync(0)
    const before = usage.now
    await vi.advanceTimersByTimeAsync(1000)
    expect(usage.now).toBeGreaterThanOrEqual(before + 1000)
    stop()
  })
})
