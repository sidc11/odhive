import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import type { Job, PythonList, SourceEntry } from '../lib/api'
import { ApiClient } from '../lib/api/client'
import { JobsStore } from '../lib/jobs.svelte'
import { ProjectsStore } from '../lib/projects.svelte'
import { ProjectWizard } from '../lib/wizard/wizard.svelte'
import { project, routedFetch } from '../test-fixtures'
import NewProject from './NewProject.svelte'

const C19: SourceEntry = { id: 'odoo-19.0', edition: 'community', odoo_version: '19.0', path: '/src/odoo-19.0' }
const PYTHONS: PythonList = {
  support: { min: [3, 10], max: [3, 14] },
  recommended: '/usr/bin/python3.12',
  interpreters: [
    { path: '/usr/bin/python3.12', version: '3.12.4', implementation: 'cpython', free_threaded: false, standard: true, compat: 'supported' },
    { path: '/usr/bin/python3.8', version: '3.8.10', implementation: 'cpython', free_threaded: false, standard: true, compat: 'too_old' },
  ],
}

function setupJob(state: Job['state']): Job {
  return { id: 9, kind: 'setup', project: 'shop', state, started_at: 1, steps: [], output: ['Installed 42 packages'], output_dropped: 0 } as Job
}

function renderWizard() {
  const { fetcher, calls } = routedFetch({
    'GET /projects': [200, []],
    'GET /sources': [200, [C19]],
    'GET /pythons?source=odoo-19.0': [200, PYTHONS],
    'POST /projects': [201, project('shop', [], { has_venv: false })],
    'POST /projects/shop/setup': [202, setupJob('running')],
  })
  const api = new ApiClient(fetcher)
  const jobs = new JobsStore()
  const wizard = new ProjectWizard(api, jobs, new ProjectsStore(api))
  render(NewProject, { wizard })
  return { calls, jobs, wizard }
}

const next = () => fireEvent.click(screen.getByRole('button', { name: 'Next' }))
const stepHeading = () => screen.getByRole('heading', { level: 2 })

describe('NewProject', () => {
  it('walks through the steps and creates and sets up the project', async () => {
    const { calls, jobs } = renderWizard()
    expect(stepHeading()).toHaveTextContent('Step 1 of 6 · Name')

    // A problem is shown only after trying to go on.
    expect(screen.queryByRole('alert')).toBeNull()
    await next()
    expect(screen.getByRole('alert')).toHaveTextContent('Enter a name.')
    await fireEvent.input(screen.getByLabelText('Project name'), { target: { value: 'shop' } })
    await next()

    expect(stepHeading()).toHaveTextContent('Odoo source')
    const source = await screen.findByRole('radio', { name: /Odoo 19\.0/ })
    expect(source).toBeChecked()
    await next()
    expect(stepHeading()).toHaveTextContent('Enterprise')
    expect(screen.getByRole('radio', { name: /None/ })).toBeChecked()
    await next()
    expect(stepHeading()).toHaveTextContent('Addons')
    await fireEvent.click(screen.getByRole('button', { name: 'Add a folder' }))
    await fireEvent.input(screen.getByLabelText('Addons folder 1'), { target: { value: '/home/dev/addons' } })
    await next()

    expect(stepHeading()).toHaveTextContent('Python')
    expect(await screen.findByRole('radio', { name: /Automatic/ })).toBeChecked()
    expect(screen.getByRole('radio', { name: /3\.8\.10/ })).toBeDisabled()
    await fireEvent.click(screen.getByRole('radio', { name: /^Python 3\.12\.4/ }))
    await next()

    expect(stepHeading()).toHaveTextContent('First instance')
    await fireEvent.input(screen.getByLabelText('HTTP port'), { target: { value: '8070' } })
    expect(screen.getByLabelText('Database')).toHaveAttribute('placeholder', 'shop_main')
    await fireEvent.click(screen.getByRole('button', { name: 'Create project' }))

    expect(await screen.findByRole('heading', { name: 'Setting up shop' })).toBeInTheDocument()
    expect(calls.find((c) => c.method === 'POST' && c.path === '/projects')?.body).toEqual({
      name: 'shop',
      community: 'odoo-19.0',
      enterprise: null,
      addons_paths: ['/home/dev/addons'],
      python: '/usr/bin/python3.12',
      instance: { id: 'main', db: null, http_port: 8070 },
    })
    expect(within(screen.getByRole('list', { name: 'Setup steps' })).getAllByRole('listitem')).toHaveLength(3)

    jobs.upsert(setupJob('succeeded'))
    expect(await screen.findByRole('heading', { name: 'shop is ready' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Open project' })).toBeInTheDocument()
  })

  it('lets finished steps be revisited from the step list', async () => {
    renderWizard()
    await fireEvent.input(screen.getByLabelText('Project name'), { target: { value: 'shop' } })
    await next()
    const steps = within(screen.getByRole('navigation', { name: 'Wizard steps' }))
    expect(steps.getByRole('button', { name: /Python/ })).toBeDisabled()
    await fireEvent.click(steps.getByRole('button', { name: /Name/ }))
    expect(stepHeading()).toHaveTextContent('Name')
    expect(steps.getByRole('button', { name: /Name/ })).toHaveAttribute('aria-current', 'step')
  })
})
