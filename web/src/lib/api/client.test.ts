import { describe, expect, expectTypeOf, it } from 'vitest'
import { project, routedFetch } from '../../test-fixtures'
import { ApiClient, ApiRequestError, UNREACHABLE } from './client'
import type { CreateProject, InstanceInfo, InstanceSettings, Job, LogPage, Project, StopResult } from './index'

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

  it('lists Pythons, judged against a source when one is given', async () => {
    const list = { support: null, recommended: null, problem: null, interpreters: [], uv_error: null }
    const { fetcher, calls } = routedFetch({ 'GET /pythons': [200, list], 'GET /pythons?source=odoo-19.0': [200, list] })
    const api = new ApiClient(fetcher)
    expect(await api.listPythons()).toEqual(list)
    await api.listPythons('odoo-19.0')
    expect(calls.map((c) => c.path)).toEqual(['/pythons', '/pythons?source=odoo-19.0'])
  })

  it('reads and replaces instance settings with PUT', async () => {
    const settings: InstanceSettings = { reload_mode: 'restart', autofix_mode: 'ask', extra_args: ['--dev=xml'], env: { A: '1' } }
    const info = { settings, project_env: ['PGHOST'], managed_options: ['--http-port'] }
    const { fetcher, calls } = routedFetch({
      'GET /projects/shop/instances/main/settings': [200, info],
      'PUT /projects/shop/instances/main/settings': [200, info],
    })
    const api = new ApiClient(fetcher)
    expect(await api.getInstanceSettings('shop', 'main')).toEqual(info)
    expect(await api.putInstanceSettings('shop', 'main', settings)).toEqual(info)
    expect(calls[1]).toMatchObject({ method: 'PUT', headers: { 'X-OdHive': '1' }, body: settings })
  })

  it('reads and replaces project options with PUT', async () => {
    const info = { options: { log_level: 'debug' }, secrets_set: [], secret_keys: ['admin_passwd'], managed_keys: [], conf_path: '/p/odoo.conf' }
    const body = { options: { log_level: 'debug' }, secrets: { admin_passwd: null } }
    const { fetcher, calls } = routedFetch({
      'GET /projects/shop/options': [200, info],
      'PUT /projects/shop/options': [200, info],
    })
    const api = new ApiClient(fetcher)
    expect(await api.getProjectOptions('shop')).toEqual(info)
    expect(await api.putProjectOptions('shop', body)).toEqual(info)
    expect(calls[1]).toMatchObject({ method: 'PUT', headers: { 'X-OdHive': '1' }, body })
  })

  it('previews, imports and reveals project options with POST', async () => {
    const preview = { text: 'a', saved: 'b' }
    const imported = { options: {}, secrets: {}, warnings: [] }
    const { fetcher, calls } = routedFetch({
      'POST /projects/shop/options/preview': [200, preview],
      'POST /projects/shop/options/import': [200, imported],
      'POST /projects/shop/options/reveal': [200, { key: 'admin_passwd', value: 'pw' }],
    })
    const api = new ApiClient(fetcher)
    expect(await api.previewProjectOptions('shop', { options: {} })).toEqual(preview)
    expect(await api.importProjectOptions('shop', { text: '[options]' })).toEqual(imported)
    expect(await api.revealProjectSecret('shop', { key: 'admin_passwd' })).toEqual({ key: 'admin_passwd', value: 'pw' })
    expect(calls.map((c) => [c.method, c.path, c.body])).toEqual([
      ['POST', '/projects/shop/options/preview', { options: {} }],
      ['POST', '/projects/shop/options/import', { text: '[options]' }],
      ['POST', '/projects/shop/options/reveal', { key: 'admin_passwd' }],
    ])
  })

  it('runs the environment check', async () => {
    const report = { package_manager: null, checks: [] }
    const { fetcher, calls } = routedFetch({ 'GET /environment': [200, report] })
    expect(await new ApiClient(fetcher).checkEnvironment()).toEqual(report)
    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual(['GET /environment'])
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

  it('sends database requests to their routes', async () => {
    const job = { id: 1 }
    const { fetcher, calls } = routedFetch({
      'GET /projects/a%20b/databases': [200, { databases: [] }],
      'POST /projects/a%20b/databases': [202, job],
      'DELETE /projects/a%20b/databases/shop.main': [202, job],
      'POST /projects/a%20b/databases/shop.main/duplicate': [202, job],
      'POST /projects/a%20b/databases/shop.main/backup': [202, job],
      'POST /projects/a%20b/databases/shop.main/snapshot': [202, job],
      'GET /projects/a%20b/backups': [200, []],
      'DELETE /projects/a%20b/backups/x.zip': [204],
      'GET /projects/a%20b/snapshots': [200, []],
      'POST /projects/a%20b/snapshots/s__snap_1/restore': [202, job],
      'DELETE /projects/a%20b/snapshots/s__snap_1': [204],
    })
    const api = new ApiClient(fetcher)
    await api.listDatabases('a b')
    await api.createDatabase('a b', { name: 'shop.main', password: 'pw' })
    await api.deleteDatabase('a b', 'shop.main', { confirm: 'shop.main' })
    await api.duplicateDatabase('a b', 'shop.main', { name: 'shop_copy' })
    await api.backupDatabase('a b', 'shop.main', { format: 'dump' })
    await api.snapshotDatabase('a b', 'shop.main', {})
    await api.listBackups('a b')
    expect(await api.deleteBackup('a b', 'x.zip')).toBeNull()
    await api.listSnapshots('a b')
    await api.restoreSnapshot('a b', 's__snap_1', { stop_instances: true })
    await api.deleteSnapshot('a b', 's__snap_1')
    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual([
      'GET /projects/a%20b/databases',
      'POST /projects/a%20b/databases',
      'DELETE /projects/a%20b/databases/shop.main',
      'POST /projects/a%20b/databases/shop.main/duplicate',
      'POST /projects/a%20b/databases/shop.main/backup',
      'POST /projects/a%20b/databases/shop.main/snapshot',
      'GET /projects/a%20b/backups',
      'DELETE /projects/a%20b/backups/x.zip',
      'GET /projects/a%20b/snapshots',
      'POST /projects/a%20b/snapshots/s__snap_1/restore',
      'DELETE /projects/a%20b/snapshots/s__snap_1',
    ])
    expect(calls[2]).toMatchObject({ headers: { 'X-OdHive': '1' }, body: { confirm: 'shop.main' } })
    expect(calls[4]?.body).toEqual({ format: 'dump' })
    expect(api.backupUrl('a b', 'x y.zip')).toBe('/api/v1/projects/a%20b/backups/x%20y.zip')
  })

  it('reads the answer of an upload like any other', async () => {
    const refused = async () => ({ status: 409, text: JSON.stringify({ error: 'database_exists', message: 'taken' }) })
    const api = new ApiClient(routedFetch({}).fetcher, refused)
    const error = await api.restoreDatabase('shop', new Blob(['x']), { name: 'shop_main' }).catch((e: unknown) => e)
    expect(error).toBeInstanceOf(ApiRequestError)
    expect(error).toMatchObject({ status: 409, code: 'database_exists', message: 'taken' })

    const unauthorized = new ApiClient(routedFetch({}).fetcher, async () => ({ status: 401, text: '' }))
    await expect(unauthorized.restoreDatabase('shop', new Blob(['x']), { name: 'shop_main' })).rejects.toMatchObject({
      code: 'unauthorized',
    })
  })
})
