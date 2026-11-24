import { describe, expect, it } from 'vitest'
import { envReport } from '../test-fixtures'
import type { EnvCheckReport } from './api'
import { ApiRequestError } from './api/client'
import { blockers, EnvironmentStore, worstStatus } from './environment.svelte'

/** An API whose answers the test releases one by one. */
function manualApi() {
  const pending: { resolve: (r: EnvCheckReport) => void; reject: (e: unknown) => void }[] = []
  const api = {
    checkEnvironment: () =>
      new Promise<EnvCheckReport>((resolve, reject) => {
        pending.push({ resolve, reject })
      }),
  }
  return { api, pending }
}

describe('worstStatus / blockers', () => {
  it('ranks ok < info < warning < error', () => {
    expect(worstStatus([])).toBe('ok')
    const report = envReport({ uv: { status: 'info' }, git: { status: 'warning' } })
    expect(worstStatus(report.checks)).toBe('warning')
    const broken = envReport({ uv: { status: 'info' }, postgresql: { status: 'error' }, git: { status: 'warning' } })
    expect(worstStatus(broken.checks)).toBe('error')
    expect(blockers(broken).map((c) => c.id)).toEqual(['postgresql'])
    expect(blockers(report)).toEqual([])
  })
})

describe('EnvironmentStore', () => {
  it('checks, keeps the last report while checking again and after a failure', async () => {
    const { api, pending } = manualApi()
    const store = new EnvironmentStore(api, () => 1234)
    expect(store.report).toBeNull()

    const first = store.check()
    expect(store.state.status).toBe('loading')
    expect(store.report).toBeNull()
    const report = envReport()
    pending[0]?.resolve(report)
    await first
    expect(store.state).toEqual({ status: 'ready', report, checkedAt: 1234 })

    const again = store.check()
    expect(store.state.status).toBe('loading')
    expect(store.report).toEqual(report)
    pending[1]?.reject(new ApiRequestError(0, 'unreachable', 'OdHive is not reachable'))
    await again
    expect(store.state).toMatchObject({ status: 'error', message: 'OdHive is not reachable' })
    expect(store.report).toEqual(report)
  })

  it('lets a newer check win and runs ensure() only once', async () => {
    const { api, pending } = manualApi()
    const store = new EnvironmentStore(api)
    void store.ensure()
    void store.ensure()
    expect(pending).toHaveLength(1)
    const newer = store.check()
    expect(pending).toHaveLength(2)

    const fresh = envReport({ git: { status: 'warning' } })
    pending[1]?.resolve(fresh)
    await newer
    pending[0]?.resolve(envReport())
    await Promise.resolve()
    expect(store.report).toEqual(fresh)

    await store.ensure()
    expect(pending).toHaveLength(2)
  })
})
