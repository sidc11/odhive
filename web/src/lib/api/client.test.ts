import { describe, expect, expectTypeOf, it } from 'vitest'
import { project, routedFetch } from '../../test-fixtures'
import { ApiClient, ApiRequestError, UNREACHABLE } from './client'
import type { CreateProject, InstanceInfo, Job, LogPage, Project, StopResult } from './index'

describe('ApiClient', () => {
  it('GETs JSON without the mutation header', async () => {
    const { fetcher, calls } = routedFetch({ 'GET /projects': [200, [project('shop')]] })
    const projects = await new ApiClient(fetcher).listProjects()
    expect(projects.map((p) => p.name)).toEqual(['shop'])
    expect(calls[0]?.headers).toEqual({ Accept: 'application/json' })
  })

  it('sends X-OdHive: 1 and a JSON body on mutations', async () => {
    const created = project('shop')
    const { fetcher, calls } = routedFetch({ 'POST /projects': [201, created] })
    const body: CreateProject = { name: 'shop', community: 'odoo-19.0' }
    expect(await new ApiClient(fetcher).createProject(body)).toEqual(created)
    expect(calls[0]).toMatchObject({
      method: 'POST',
      headers: { 'X-OdHive': '1', 'Content-Type': 'application/json' },
      body,
    })
  })

  it('encodes path segments and query parameters', async () => {
    const page: LogPage = { records: [], next: 5, skipped: 0 }
    const { fetcher, calls } = routedFetch({
      'GET /projects/a%20b/instances/m%2Fx/logs?from=3&limit=10': [200, page],
      'POST /projects/a%20b/instances/m%2Fx/stop': [200, { was_running: false, killed: false }],
    })
    const api = new ApiClient(fetcher)
    expect(await api.instanceLogs('a b', 'm/x', { from: 3, limit: 10 })).toEqual(page)
    await api.stopInstance('a b', 'm/x')
    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual([
      'GET /projects/a%20b/instances/m%2Fx/logs?from=3&limit=10',
      'POST /projects/a%20b/instances/m%2Fx/stop',
    ])
  })

  it('reads instance usage', async () => {
    const usage = [
      { project: 'shop', instance: 'main', pid: 7, cpu_percent: 1.5, memory_bytes: 1024, processes: 2, started_at: 100 },
    ]
    const { fetcher } = routedFetch({ 'GET /instances/usage': [200, usage] })
    expect(await new ApiClient(fetcher).instanceUsage()).toEqual(usage)
  })

  it('accepts empty answers (204, 202)', async () => {
    const { fetcher } = routedFetch({ 'DELETE /projects/shop': [204], 'POST /service/quit': [202] })
    const api = new ApiClient(fetcher)
    await expect(api.removeProject('shop')).resolves.toBeNull()
    await expect(api.quit()).resolves.toBeNull()
  })

  it('turns error bodies into ApiRequestError with the stable code', async () => {
    const { fetcher } = routedFetch({
      'POST /projects/shop/instances/main/start': [409, { error: 'already_running', message: 'it runs' }],
    })
    const error = await new ApiClient(fetcher).startInstance('shop', 'main').catch((e: unknown) => e)
    expect(error).toBeInstanceOf(ApiRequestError)
    expect(error).toMatchObject({ status: 409, code: 'already_running', message: 'it runs', unauthorized: false })
  })

  it('explains a missing sign-in, bare errors and an unreachable service', async () => {
    const unauthorized = new ApiClient(routedFetch({ 'GET /jobs': [401, { error: 'unauthorized', message: 'x' }] }).fetcher)
    await expect(unauthorized.listJobs()).rejects.toMatchObject({
      status: 401,
      unauthorized: true,
      message: expect.stringContaining('odhive'),
    })

    const bare = new ApiClient(async () => new Response('<html>bad gateway</html>', { status: 502 }))
    await expect(bare.health()).rejects.toMatchObject({ status: 502, code: 'http_error', message: expect.stringContaining('502') })

    const down = new ApiClient(async () => {
      throw new TypeError('Failed to fetch')
    })
    await expect(down.health()).rejects.toMatchObject({ status: 0, code: UNREACHABLE })
  })

  it('has response types from the generated schema', () => {
    const api = new ApiClient(routedFetch({}).fetcher)
    expectTypeOf(api.listProjects).returns.resolves.toEqualTypeOf<Project[]>()
    expectTypeOf(api.startInstance).returns.resolves.toEqualTypeOf<InstanceInfo>()
    expectTypeOf(api.stopInstance).returns.resolves.toEqualTypeOf<StopResult>()
    expectTypeOf(api.cloneSource).returns.resolves.toEqualTypeOf<Job>()
    expectTypeOf(api.createProject).parameter(0).toEqualTypeOf<CreateProject>()
  })
})
