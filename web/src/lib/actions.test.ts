import { describe, expect, it } from 'vitest'
import { instanceView, live, project, routedFetch } from '../test-fixtures'
import { InstanceActions } from './actions.svelte'
import { ApiClient } from './api/client'
import { ProjectsStore } from './projects.svelte'
import { ToastStore } from './toasts.svelte'

const SHOP = project('shop', [instanceView('main')])
const BASE = '/projects/shop/instances/main'

function setup(routes: Parameters<typeof routedFetch>[0]) {
  // The same object, so a test can change a route later.
  routes['GET /projects'] ??= [200, [SHOP]]
  const { fetcher, calls } = routedFetch(routes)
  const api = new ApiClient(fetcher)
  const projects = new ProjectsStore(api)
  const toasts = new ToastStore()
  return { actions: new InstanceActions(api, projects, toasts), projects, toasts, calls }
}

function liveOf(projects: ProjectsStore) {
  return projects.find('shop')?.instances[0]?.live
}

describe('InstanceActions', () => {
  it('records what start returns and is busy meanwhile', async () => {
    const started = live('shop', 'main', { state: 'starting', pid: 4 })
    const { actions, projects, calls } = setup({ [`POST ${BASE}/start`]: [200, started] })
    await projects.load()
    const running = actions.run('start', 'shop', 'main')
    expect(actions.busy('shop', 'main')).toBe('start')
    // A second click while busy sends nothing.
    await actions.run('start', 'shop', 'main')
    await running
    expect(actions.busy('shop', 'main')).toBeUndefined()
    expect(liveOf(projects)).toEqual(started)
    expect(calls.filter((c) => c.method === 'POST')).toHaveLength(1)
  })

  it('restarts through the restart route', async () => {
    const info = live('shop', 'main', { state: 'starting', pid: 5 })
    const { actions, projects } = setup({ [`POST ${BASE}/restart`]: [200, info] })
    await projects.load()
    await actions.run('restart', 'shop', 'main')
    expect(liveOf(projects)?.status).toEqual({ state: 'starting', pid: 5 })
  })

  it('turns failures into one error toast and reloads after a conflict', async () => {
    const { actions, toasts, calls } = setup({
      [`POST ${BASE}/start`]: [409, { error: 'running_elsewhere', message: 'shop/main runs in another OdHive' }],
    })
    await actions.run('start', 'shop', 'main')
    await actions.run('start', 'shop', 'main')
    expect(toasts.items).toHaveLength(1)
    expect(toasts.items[0]).toMatchObject({
      kind: 'error',
      title: 'Could not start shop/main',
      body: 'shop/main runs in another OdHive',
    })
    expect(calls.filter((c) => c.path === '/projects')).toHaveLength(2)
  })

  it('warns when a stop had to kill, and reloads after stopping an external instance', async () => {
    const { actions, toasts, calls } = setup({ [`POST ${BASE}/stop`]: [200, { was_running: true, killed: true, status: null }] })
    await actions.run('stop', 'shop', 'main')
    expect(toasts.items[0]).toMatchObject({ kind: 'warning', title: 'shop/main was killed' })
    expect(calls.some((c) => c.path === '/projects')).toBe(true)
  })

  it('clears the previous failure when an action succeeds', async () => {
    const routes: Parameters<typeof routedFetch>[0] = { [`POST ${BASE}/stop`]: [500, { error: 'internal', message: 'x' }] }
    const { actions, toasts } = setup(routes)
    await actions.run('stop', 'shop', 'main')
    expect(toasts.items).toHaveLength(1)
    routes[`POST ${BASE}/stop`] = [200, { was_running: true, killed: false, status: { state: 'stopped', exit: { port_in_use: false, tail: '' } } }]
    await actions.run('stop', 'shop', 'main')
    expect(toasts.items).toHaveLength(0)
  })
})
