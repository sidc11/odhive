import { describe, expect, it } from 'vitest'
import { project, routedFetch } from '../../test-fixtures'
import type { Job, PythonList, SourceEntry } from '../api'
import { ApiClient } from '../api/client'
import { JobsStore } from '../jobs.svelte'
import { ProjectsStore } from '../projects.svelte'
import { dbProblem, nameProblem, pathProblem, portProblem, ProjectWizard, STEPS, stepOfError } from './wizard.svelte'

const C19: SourceEntry = { id: 'odoo-19.0', edition: 'community', odoo_version: '19.0', path: '/src/odoo-19.0' }
const C20: SourceEntry = { id: 'odoo-20.0', edition: 'community', odoo_version: '20.0', path: '/src/odoo-20.0' }
const E19: SourceEntry = { id: 'enterprise-19.0', edition: 'enterprise', odoo_version: '19.0', path: '/src/ent-19' }

const PYTHONS: PythonList = {
  support: { min: [3, 10], max: [3, 14] },
  recommended: '/usr/bin/python3.12',
  interpreters: [
    { path: '/usr/bin/python3.12', version: '3.12.4', implementation: 'cpython', free_threaded: false, standard: true, compat: 'supported' },
  ],
}

function job(id: number, extra: Partial<Job> = {}): Job {
  return { id, kind: 'setup', project: 'shop', state: 'running', started_at: 1, steps: [], output: [], output_dropped: 0, ...extra } as Job
}

function setup(routes: Parameters<typeof routedFetch>[0] = {}) {
  routes['GET /projects'] ??= [200, [project('taken')]]
  routes['GET /sources'] ??= [200, [C19, C20, E19]]
  routes['GET /pythons?source=odoo-19.0'] ??= [200, PYTHONS]
  const { fetcher, calls } = routedFetch(routes)
  const api = new ApiClient(fetcher)
  const jobs = new JobsStore()
  const projects = new ProjectsStore(api)
  return { wizard: new ProjectWizard(api, jobs, projects), jobs, projects, calls }
}

/** Fills every step with valid answers, ending on the last step. */
async function fillIn(wizard: ProjectWizard): Promise<void> {
  wizard.name = 'shop'
  expect(wizard.next()).toBe(true)
  await wizard.loadSources()
  wizard.chooseCommunity('odoo-19.0')
  while (!wizard.isLast) expect(wizard.next()).toBe(true)
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0))

describe('field checks', () => {
  it('match the server rules', () => {
    expect(nameProblem('shop')).toBeNull()
    expect(nameProblem('')).toMatch(/Enter/)
    for (const bad of ['Shop', '-x', 'a b', '../x', 'a'.repeat(65)]) expect(nameProblem(bad)).not.toBeNull()
    expect(nameProblem('shop', ['shop'])).toMatch(/exists/)
    expect(pathProblem('/a')).toBeNull()
    expect(pathProblem('rel/a')).toMatch(/absolute/)
    expect(dbProblem('')).toBeNull()
    expect(dbProblem('Shop-19.0_x')).toBeNull()
    expect(dbProblem('a;b')).not.toBeNull()
    expect(portProblem('')).toBeNull()
    expect(portProblem('8070')).toBeNull()
    for (const bad of ['80', '65536', '80.5', '1e4', 'x']) expect(portProblem(bad)).not.toBeNull()
  })

  it('send create errors back to their step', () => {
    expect(stepOfError('exists')).toBe('name')
    expect(stepOfError('bad_addons_path')).toBe('addons')
    expect(stepOfError('invalid_project')).toBe('instance')
  })
})

describe('ProjectWizard', () => {
  it('only moves on from a finished step, and back only to reached ones', async () => {
    const { wizard, projects } = setup()
    await projects.load()
    expect(wizard.next()).toBe(false)
    wizard.name = 'taken'
    expect(wizard.problem()).toMatch(/exists/)
    wizard.name = 'shop'
    expect(wizard.next()).toBe(true)
    expect(wizard.stepId).toBe('source')
    wizard.goto(4)
    expect(wizard.stepId).toBe('source')
    wizard.back()
    expect(wizard.stepId).toBe('name')
    wizard.goto(1)
    expect(wizard.stepId).toBe('source')
  })

  it('loads sources, picks the newest series and drops an Enterprise tree of another one', async () => {
    const { wizard } = setup()
    wizard.name = 'shop'
    wizard.next()
    await wizard.loadSources()
    expect(wizard.community).toBe('odoo-20.0')
    expect(wizard.enterpriseSources).toEqual([])
    wizard.chooseCommunity('odoo-19.0')
    expect(wizard.enterpriseSources).toEqual([E19])
    wizard.enterprise = 'enterprise-19.0'
    expect(wizard.problem('enterprise')).toBeNull()
    wizard.chooseCommunity('odoo-20.0')
    expect(wizard.enterprise).toBeNull()
  })

  it('registers a Community folder and chooses it', async () => {
    const C = { ...C19, id: 'odoo-19.0-2', path: '/x/odoo' }
    const { wizard, calls } = setup({
      'GET /sources': [200, [C19]],
      'POST /sources': [201, C],
    })
    await wizard.loadSources()
    expect(await wizard.register('relative', 'community')).toBe(false)
    expect(wizard.sourceError).toMatch(/absolute/)
    expect(await wizard.register('/x/odoo', 'community')).toBe(true)
    expect(wizard.community).toBe(C.id)
    expect(wizard.sourceError).toBeNull()
    expect(calls.at(-1)?.body).toEqual({ path: '/x/odoo' })
    expect(wizard.communitySources.map((s) => s.id)).toEqual(['odoo-19.0', C.id])
  })

  it('registers an Enterprise folder for the chosen series', async () => {
    const E = { ...E19, id: 'enterprise-19.0-2', path: '/x/ent' }
    const { wizard, calls } = setup({ 'POST /sources': [201, E] })
    await wizard.loadSources()
    wizard.chooseCommunity('odoo-19.0')
    expect(await wizard.register('/x/ent', 'enterprise')).toBe(true)
    expect(calls.at(-1)?.body).toEqual({ path: '/x/ent', enterprise_version: '19.0' })
    expect(wizard.enterprise).toBe(E.id)
    expect(wizard.enterpriseSources.map((s) => s.id)).toContain(E.id)
  })

  it('shows why a registration failed', async () => {
    const { wizard } = setup({
      'POST /sources': [422, { error: 'not_odoo', message: '/x is not an Odoo 19/20 source tree' }],
    })
    await wizard.loadSources()
    expect(await wizard.register('/x', 'community')).toBe(false)
    expect(wizard.sourceError).toBe('/x is not an Odoo 19/20 source tree')
  })

  it('clones Community, waits for the job and chooses the new source', async () => {
    const cloned: SourceEntry = { ...C20, id: 'odoo-20.0', path: '/ws/sources/odoo-20.0' }
    const { wizard, jobs } = setup({
      'GET /sources': [200, [C19]],
      'POST /sources/clone': [202, job(7, { kind: 'clone', version: '20.0' } as Partial<Job>)],
    })
    await wizard.loadSources()
    const cloning = wizard.clone('20.0')
    await tick()
    expect(wizard.cloneJob).toBe(7)
    expect(wizard.problem('source')).toMatch(/Wait/)
    jobs.apply({
      type: 'job_events',
      events: [
        { event: 'finished', job: job(7, { kind: 'clone', version: '20.0', state: 'succeeded', result: { kind: 'clone', source: cloned } } as Partial<Job>) },
      ],
    })
    expect(await cloning).toBe(true)
    expect(wizard.cloneJob).toBeNull()
    expect(wizard.community).toBe('odoo-20.0')
    expect(wizard.communitySources.map((s) => s.id)).toEqual(['odoo-19.0', 'odoo-20.0'])
  })

  it('reports a failed clone and frees the step', async () => {
    const { wizard, jobs } = setup({ 'POST /sources/clone': [202, job(8, { kind: 'clone', version: '19.0' } as Partial<Job>)] })
    await wizard.loadSources()
    const cloning = wizard.clone('19.0')
    await tick()
    jobs.upsert(job(8, { kind: 'clone', version: '19.0', state: 'failed', error: { error: 'clone_failed', message: 'git exited with 128' } } as Partial<Job>))
    expect(await cloning).toBe(false)
    expect(wizard.sourceError).toBe('git exited with 128')
    expect(wizard.cloneJob).toBeNull()
  })

  it('loads Pythons for the chosen source once', async () => {
    const { wizard, calls } = setup()
    await fillIn(wizard)
    await tick()
    expect(wizard.pythons).toEqual({ status: 'ready', value: PYTHONS })
    await wizard.loadPythons()
    expect(calls.filter((c) => c.path.startsWith('/pythons'))).toHaveLength(1)
  })

  it('checks addons and instance fields', async () => {
    const { wizard } = setup()
    await fillIn(wizard)
    wizard.addAddons('relative/path')
    expect(wizard.problem('addons')).toMatch(/absolute/)
    wizard.addons[0] = '/home/dev/addons'
    expect(wizard.problem('addons')).toBeNull()
    wizard.httpPort = '80'
    expect(wizard.problem('instance')).toMatch(/port/)
    wizard.httpPort = ''
    wizard.instanceId = 'Main'
    expect(wizard.problem('instance')).toMatch(/instance id/)
    wizard.instanceId = 'dev'
    expect(wizard.defaultDb).toBe('shop_dev')
  })

  it('creates the project, sets it up and follows the job to the end', async () => {
    const created = project('shop', [], { has_venv: false })
    const { wizard, jobs, calls } = setup({
      'POST /projects': [201, created],
      'POST /projects/shop/setup': [202, job(3)],
    })
    await fillIn(wizard)
    wizard.addAddons('/home/dev/addons')
    wizard.addAddons('')
    wizard.python = '/usr/bin/python3.12'
    wizard.installRequirements = false
    wizard.httpPort = '8070'
    wizard.db = 'shop_db'

    const creating = wizard.create()
    await tick()
    await tick()
    expect(wizard.phase).toMatchObject({ name: 'setting_up', job: 3 })
    jobs.upsert(job(3, { state: 'succeeded' }))
    await creating
    expect(wizard.phase).toMatchObject({ name: 'done', job: 3 })

    expect(calls.find((c) => c.method === 'POST' && c.path === '/projects')?.body).toEqual({
      name: 'shop',
      community: 'odoo-19.0',
      enterprise: null,
      addons_paths: ['/home/dev/addons'],
      python: '/usr/bin/python3.12',
      instance: { id: 'main', db: 'shop_db', http_port: 8070 },
    })
    expect(calls.find((c) => c.path === '/projects/shop/setup')?.body).toEqual({ skip_requirements: true })
    // The sidebar learns about the project.
    expect(calls.filter((c) => c.method === 'GET' && c.path === '/projects').length).toBeGreaterThan(0)
  })

  it('skips the setup when asked to', async () => {
    const { wizard, calls } = setup({ 'POST /projects': [201, project('shop')] })
    await fillIn(wizard)
    wizard.setupNow = false
    await wizard.create()
    expect(wizard.phase).toMatchObject({ name: 'done', job: null })
    expect(calls.some((c) => c.path.endsWith('/setup'))).toBe(false)
  })

  it('goes back to the step a create error belongs to', async () => {
    const { wizard } = setup({
      'POST /projects': [422, { error: 'bad_addons_path', message: '/gone is not a folder' }],
    })
    await fillIn(wizard)
    await wizard.create()
    expect(wizard.phase).toEqual({ name: 'editing' })
    expect(wizard.stepId).toBe('addons')
    expect(wizard.createError).toBe('/gone is not a folder')
  })

  it('jumps to the first unfinished step instead of creating', async () => {
    const { wizard, calls } = setup()
    await fillIn(wizard)
    wizard.name = ''
    await wizard.create()
    expect(wizard.stepId).toBe('name')
    expect(calls.some((c) => c.method === 'POST')).toBe(false)
  })

  it('keeps the created project when the setup fails, and can retry', async () => {
    const routes: Parameters<typeof routedFetch>[0] = {
      'POST /projects': [201, project('shop')],
      'POST /projects/shop/setup': [202, job(4)],
    }
    const { wizard, jobs } = setup(routes)
    await fillIn(wizard)
    const creating = wizard.create()
    await tick()
    await tick()
    jobs.upsert(job(4, { state: 'failed', error: { error: 'requirements_failed', message: 'psycopg2 did not build' } }))
    await creating
    expect(wizard.phase).toMatchObject({ name: 'setup_failed', job: 4, message: 'psycopg2 did not build' })

    routes['POST /projects/shop/setup'] = [202, job(5)]
    const phase = wizard.phase
    if (phase.name !== 'setup_failed') throw new Error('unexpected phase')
    const retry = wizard.setup(phase.project)
    await tick()
    jobs.upsert(job(5, { state: 'succeeded' }))
    await retry
    expect(wizard.phase).toMatchObject({ name: 'done', job: 5 })
  })

  it('cancels a running setup through the job', async () => {
    const { wizard, jobs, calls } = setup({
      'POST /projects': [201, project('shop')],
      'POST /projects/shop/setup': [202, job(6)],
      'POST /jobs/6/cancel': [200, job(6)],
    })
    await fillIn(wizard)
    const creating = wizard.create()
    await tick()
    await tick()
    await wizard.cancelSetup()
    expect(calls.some((c) => c.path === '/jobs/6/cancel')).toBe(true)
    jobs.upsert(job(6, { state: 'cancelled' }))
    await creating
    expect(wizard.phase).toMatchObject({ name: 'setup_failed', message: 'The setup was cancelled.' })
  })

  it('covers every step', () => {
    expect(STEPS).toEqual(['name', 'source', 'enterprise', 'addons', 'python', 'instance'])
  })
})
