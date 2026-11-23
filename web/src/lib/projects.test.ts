import { describe, expect, it, vi } from 'vitest'
import { instanceView, live, project, routedFetch } from '../test-fixtures'
import type { InstanceInfo, Project } from './api'
import { ApiClient } from './api/client'
import { ProjectsStore } from './projects.svelte'

function store(routes: Parameters<typeof routedFetch>[0]) {
  const { fetcher, calls } = routedFetch(routes)
  return { projects: new ProjectsStore(new ApiClient(fetcher)), calls }
}

function liveOf(s: ProjectsStore, name: string, id: string): InstanceInfo | null | undefined {
  return s.find(name)?.instances.find((view) => view.id === id)?.live
}

const SHOP: Project = project('shop', [instanceView('main'), instanceView('test')])

describe('ProjectsStore', () => {
  it('loads the project list from the API', async () => {
    const { projects, calls } = store({ 'GET /projects': [200, [SHOP]] })
    expect(projects.state.status).toBe('loading')
    await projects.load()
    expect(calls.map((c) => c.path)).toEqual(['/projects'])
    expect(projects.state).toMatchObject({ status: 'ready', projects: [{ name: 'shop' }] })
    expect(projects.find('shop')?.name).toBe('shop')
    expect(projects.find('other')).toBeUndefined()
  })

  it('runs one more request for loads during a request, shared by them', async () => {
    const { projects, calls } = store({ 'GET /projects': [200, [SHOP]] })
    await Promise.all([projects.load(), projects.load(), projects.load()])
    expect(calls).toHaveLength(2)
    await projects.load()
    expect(calls).toHaveLength(3)
  })

  it('explains a missing sign-in, also over a loaded list', async () => {
    const { projects } = store({ 'GET /projects': [401, { error: 'unauthorized', message: 'sign in' }] })
    await projects.load()
    expect(projects.state).toMatchObject({ status: 'error', unauthorized: true })
    expect(projects.state.status === 'error' && projects.state.message).toContain('odhive')
  })

  it('shows the API error message', async () => {
    const { projects } = store({ 'GET /projects': [500, { error: 'internal', message: 'disk on fire' }] })
    await projects.load()
    expect(projects.state).toMatchObject({ status: 'error', message: 'disk on fire', unauthorized: false })
  })

  it('keeps a loaded list when a reload cannot reach the service', async () => {
    let up = true
    const fetcher = vi.fn<typeof fetch>(async () => {
      if (!up) throw new TypeError('network')
      return new Response(JSON.stringify([SHOP]))
    })
    const projects = new ProjectsStore(new ApiClient(fetcher))
    await projects.load()
    up = false
    await projects.load()
    expect(projects.state.status).toBe('ready')

    const fresh = new ProjectsStore(new ApiClient(fetcher))
    await fresh.load()
    expect(fresh.state).toMatchObject({ status: 'error', message: expect.stringContaining('not reachable') })
  })

  it('takes live state from a snapshot, replacing the old one', async () => {
    const running = live('shop', 'main', { state: 'running', pid: 7 })
    const { projects } = store({
      'GET /projects': [200, [project('shop', [instanceView('main', { live: running }), instanceView('test')])]],
    })
    await projects.load()
    expect(liveOf(projects, 'shop', 'main')?.status.state).toBe('running')

    projects.apply({ type: 'snapshot', instances: [live('shop', 'test', { state: 'starting', pid: 8 })] })
    expect(liveOf(projects, 'shop', 'main')).toBeNull()
    expect(liveOf(projects, 'shop', 'test')?.status).toEqual({ state: 'starting', pid: 8 })
  })

  it('applies spawned and status events to known instances', async () => {
    const { projects } = store({ 'GET /projects': [200, [SHOP]] })
    await projects.load()
    const stale = vi.fn()
    projects.onstale = stale
    projects.apply({ type: 'snapshot', instances: [live('shop', 'main', { state: 'running', pid: 7 })] })
    projects.apply({
      type: 'events',
      events: [
        { project: 'shop', instance: 'main', event: 'spawned', pid: 9, http_port: 8070, gevent_port: 8073, changes: [] },
        { project: 'shop', instance: 'main', event: 'output', line: 'x', stream: 'stdout' },
      ],
    })
    expect(liveOf(projects, 'shop', 'main')).toMatchObject({
      http_port: 8070,
      gevent_port: 8073,
      status: { state: 'starting', pid: 9 },
    })
    projects.apply({
      type: 'events',
      events: [{ project: 'shop', instance: 'main', event: 'status', status: { state: 'running', pid: 9 } }],
    })
    expect(liveOf(projects, 'shop', 'main')?.status.state).toBe('running')
    expect(stale).not.toHaveBeenCalled()
  })

  it('keeps port moves from the first spawn across restarts', async () => {
    const { projects } = store({ 'GET /projects': [200, [SHOP]] })
    await projects.load()
    const moved = { kind: 'http', requested: 8069, assigned: 8070, reason: { reason: 'in_use' } } as const
    const spawned = (changes: (typeof moved)[]) => ({
      type: 'events' as const,
      events: [
        { project: 'shop', instance: 'main', event: 'spawned' as const, pid: 9, http_port: 8070, gevent_port: 8072, changes },
      ],
    })
    projects.apply(spawned([moved]))
    expect(liveOf(projects, 'shop', 'main')?.port_changes).toEqual([moved])
    projects.apply(spawned([]))
    expect(liveOf(projects, 'shop', 'main')?.port_changes).toEqual([moved])
  })

  it('shows an unknown instance at once and asks for a snapshot', async () => {
    const { projects } = store({ 'GET /projects': [200, [SHOP]] })
    await projects.load()
    const stale = vi.fn()
    projects.onstale = stale
    projects.apply({
      type: 'events',
      events: [{ project: 'shop', instance: 'test', event: 'status', status: { state: 'running', pid: 3 } }],
    })
    expect(liveOf(projects, 'shop', 'test')).toMatchObject({ db: 'db_test', status: { state: 'running', pid: 3 } })
    expect(stale).toHaveBeenCalledTimes(1)
  })

  it('asks for a snapshot after notices that change instance details', async () => {
    const { projects } = store({ 'GET /projects': [200, [SHOP]] })
    await projects.load()
    projects.apply({ type: 'snapshot', instances: [live('shop', 'main', { state: 'crashed', exit: { port_in_use: false, tail: '' } })] })
    const stale = vi.fn()
    projects.onstale = stale
    projects.apply({
      type: 'events',
      events: [
        { project: 'shop', instance: 'main', event: 'reload', action: { kind: 'restart' }, file_count: 1, files: ['a.py'] },
      ],
    })
    expect(stale).not.toHaveBeenCalled()
    projects.apply({
      type: 'events',
      events: [
        {
          project: 'shop',
          instance: 'main',
          event: 'dependency_missing',
          module: 'zz',
          package: 'zz',
          action: { kind: 'confirm' },
        },
      ],
    })
    expect(stale).toHaveBeenCalledTimes(1)
  })

  it('does not let an older REST answer undo newer WebSocket state', async () => {
    let answer: (response: Response) => void = () => {}
    const fetcher: typeof fetch = () => new Promise((resolve) => (answer = resolve))
    const projects = new ProjectsStore(new ApiClient(fetcher))
    const loading = projects.load()
    projects.apply({ type: 'snapshot', instances: [live('shop', 'main', { state: 'running', pid: 7 })] })
    answer(new Response(JSON.stringify([SHOP])))
    await loading
    expect(liveOf(projects, 'shop', 'main')?.status.state).toBe('running')
  })

  it('records what a start returned', async () => {
    const { projects } = store({ 'GET /projects': [200, [SHOP]] })
    await projects.load()
    projects.upsert(live('shop', 'test', { state: 'starting', pid: 11 }))
    expect(liveOf(projects, 'shop', 'test')?.status.state).toBe('starting')
  })

  it('ignores messages not about instances', async () => {
    const { projects } = store({ 'GET /projects': [200, [SHOP]] })
    await projects.load()
    const before = projects.state
    projects.apply({ type: 'hello', version: '0.1.0' })
    projects.apply({ type: 'job_snapshot', jobs: [] })
    expect(projects.state).toEqual(before)
  })
})
