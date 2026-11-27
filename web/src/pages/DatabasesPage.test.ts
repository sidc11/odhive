import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import { ApiClient } from '../lib/api/client'
import { JobsStore } from '../lib/jobs.svelte'
import { backup, database, databaseJob, databaseList, envReport, routedFetch, snapshot } from '../test-fixtures'
import DatabasesPage from './DatabasesPage.svelte'

const SNAP = 'shop_main__snap_20261126090500'

function renderPage(routes: Record<string, [number, unknown?]> = {}, jobs = new JobsStore()) {
  const { fetcher, calls } = routedFetch({
    'GET /projects/shop/databases': [
      200,
      databaseList([database('shop_main', { instances: ['main'] }), database('shop_test', { size_bytes: null, odoo: false })]),
    ],
    'GET /projects/shop/backups': [200, [backup('shop_main_20261126-090500.zip')]],
    'GET /projects/shop/snapshots': [200, [snapshot(SNAP), snapshot('shop_old__snap_20261101000000', { broken: true })]],
    ...routes,
  })
  render(DatabasesPage, { project: 'shop', api: new ApiClient(fetcher), jobs })
  return { calls, jobs }
}

const row = (name: string) => screen.getByRole('row', { name: new RegExp(`^${name}`) })
const dialog = () => screen.getByRole('dialog')

describe('DatabasesPage', () => {
  it('reads the PostgreSQL server log only while it is shown', async () => {
    const pgLog = {
      path: '/var/log/postgresql/postgresql-16-main.log',
      configured: null,
      found: ['/var/log/postgresql/postgresql-16-main.log'],
      status: 'ok',
      message: null,
      hint: null,
      stream: 1,
      records: [],
      skipped: 0,
      next: 1,
    }
    const { calls } = renderPage({ 'GET /projects/shop/postgres/log?limit=1000': [200, pgLog] })
    const toggle = screen.getByRole('button', { name: 'Show log' })
    expect(toggle).toHaveAttribute('aria-expanded', 'false')
    expect(calls.some((c) => c.path.includes('postgres/log'))).toBe(false)
    await fireEvent.click(toggle)
    expect(await screen.findByTestId('postgres-log-path')).toHaveTextContent('postgresql-16-main.log')
    await fireEvent.click(screen.getByRole('button', { name: 'Hide log' }))
    expect(screen.queryByTestId('postgres-log-path')).not.toBeInTheDocument()
  })

  it('lists the databases, snapshots and backups', async () => {
    renderPage()
    const main = await vi.waitFor(() => row('shop_main'))
    expect(within(main).getByText('19.0.1.3')).toBeInTheDocument()
    expect(within(main).getByText('12 MiB')).toBeInTheDocument()
    expect(within(main).getByRole('link', { name: 'main' })).toHaveAttribute('href', '/projects/shop/instances/main')
    expect(within(row('shop_test')).getByText('Not Odoo')).toBeInTheDocument()

    const snap = screen.getByRole('group', { name: `Actions for snapshot ${SNAP}` })
    expect(within(snap).getByRole('button', { name: 'Restore' })).toBeEnabled()
    const broken = screen.getByRole('group', { name: 'Actions for snapshot shop_old__snap_20261101000000' })
    expect(within(broken).getByRole('button', { name: 'Restore' })).toBeDisabled()
    expect(within(broken).getByRole('button', { name: 'Delete' })).toBeEnabled()
    expect(screen.getByText(/can only be deleted/)).toBeInTheDocument()

    const download = screen.getByRole('link', { name: 'Download' })
    expect(download).toHaveAttribute('href', '/api/v1/projects/shop/backups/shop_main_20261126-090500.zip')
    expect(download).toHaveAttribute('download', 'shop_main_20261126-090500.zip')
  })

  it('deletes after the exact name is typed, then follows the job', async () => {
    const job = databaseJob(5, 'delete_database', 'shop_test')
    const { calls, jobs } = renderPage({ 'DELETE /projects/shop/databases/shop_test': [202, job] })
    await fireEvent.click(within(await vi.waitFor(() => row('shop_test'))).getByRole('button', { name: 'Delete' }))

    const confirm = within(dialog()).getByLabelText(/to confirm/)
    const submit = within(dialog()).getByRole('button', { name: 'Delete' })
    expect(submit).toBeDisabled()
    await fireEvent.input(confirm, { target: { value: 'SHOP_TEST' } })
    expect(submit).toBeDisabled()
    await fireEvent.input(confirm, { target: { value: 'shop_test' } })
    await fireEvent.click(within(dialog()).getByLabelText(/Take a snapshot first/))
    await fireEvent.click(submit)

    await vi.waitFor(() => expect(within(dialog()).getByRole('button', { name: 'Cancel job' })).toBeInTheDocument())
    expect(within(dialog()).getByRole('heading', { name: 'Delete database' })).toBeInTheDocument()
    expect(calls.find((c) => c.method === 'DELETE')?.body).toEqual({ confirm: 'shop_test', snapshot: false })

    jobs.apply({ type: 'job_events', events: [{ event: 'step', id: 5, step: { job: 'delete_database', event: 'step', step: 'drop' } }] })
    await vi.waitFor(() =>
      expect(within(dialog()).getByText('Drop the database and its filestore')).toBeInTheDocument(),
    )
    const listsBefore = calls.filter((c) => c.path === '/projects/shop/databases').length
    jobs.apply({
      type: 'job_events',
      events: [
        {
          event: 'finished',
          job: {
            ...job,
            state: 'succeeded',
            steps: [{ job: 'delete_database', event: 'step', step: 'drop' }],
            result: { kind: 'delete_database', database: { name: 'shop_test', filestore_removed: true } },
          },
        },
      ],
    })
    await vi.waitFor(() => expect(screen.getByTestId('job-outcome')).toHaveTextContent('Deleted shop_test.'))
    expect(within(dialog()).queryByRole('button', { name: 'Cancel job' })).toBeNull()
    await vi.waitFor(() => expect(calls.filter((c) => c.path === '/projects/shop/databases').length).toBe(listsBefore + 1))

    await fireEvent.click(within(dialog()).getByRole('button', { name: 'Close' }))
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('checks the create form before sending it', async () => {
    const { calls } = renderPage({ 'POST /projects/shop/databases': [202, databaseJob(6, 'create_database', 'shop_new')] })
    await fireEvent.click(await screen.findByRole('button', { name: 'New database…' }))
    await fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }))
    expect(within(dialog()).getByText('Enter a name.')).toBeInTheDocument()
    expect(within(dialog()).getByText('Enter a password.')).toBeInTheDocument()
    expect(calls.some((c) => c.method === 'POST')).toBe(false)

    await fireEvent.input(within(dialog()).getByLabelText('Database name'), { target: { value: 'shop_new' } })
    await fireEvent.input(within(dialog()).getByLabelText('Admin password'), { target: { value: 'secret' } })
    await fireEvent.input(within(dialog()).getByLabelText('Modules to install'), { target: { value: 'sale, Bad' } })
    await fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }))
    expect(within(dialog()).getByText('Not a module name: Bad')).toBeInTheDocument()
    expect(calls.some((c) => c.method === 'POST')).toBe(false)

    await fireEvent.input(within(dialog()).getByLabelText('Modules to install'), { target: { value: 'sale stock' } })
    await fireEvent.input(within(dialog()).getByLabelText('Country'), { target: { value: 'BE' } })
    await fireEvent.click(within(dialog()).getByRole('button', { name: 'Create' }))
    await vi.waitFor(() => expect(calls.some((c) => c.method === 'POST')).toBe(true))
    expect(calls.find((c) => c.method === 'POST')?.body).toEqual({
      name: 'shop_new',
      password: 'secret',
      demo: false,
      modules: ['sale', 'stock'],
      language: null,
      country: 'BE',
      login: null,
    })
  })

  it('offers to stop the instances using a database it copies, and shows a refusal', async () => {
    const { calls } = renderPage({
      'POST /projects/shop/databases/shop_main/duplicate': [409, { error: 'database_in_use', message: 'main runs on shop_main' }],
    })
    await fireEvent.click(within(await vi.waitFor(() => row('shop_main'))).getByRole('button', { name: 'Duplicate' }))
    expect(within(dialog()).getByLabelText('Name of the copy')).toHaveValue('shop_main_copy')
    expect(within(dialog()).getByText(/main runs on it/)).toBeInTheDocument()
    await fireEvent.click(within(dialog()).getByRole('button', { name: 'Duplicate' }))
    await vi.waitFor(() => expect(within(dialog()).getByRole('alert')).toHaveTextContent('main runs on shop_main'))
    expect(calls.at(-1)?.body).toEqual({ name: 'shop_main_copy', neutralize: false, stop_instances: false })

    await fireEvent.click(within(dialog()).getByLabelText(/Stop the instances using it/))
    await fireEvent.click(within(dialog()).getByRole('button', { name: 'Duplicate' }))
    await vi.waitFor(() => expect(calls.at(-1)?.body).toMatchObject({ stop_instances: true }))
  })

  it('shows a job started elsewhere and blocks actions on its database', async () => {
    const jobs = new JobsStore()
    jobs.upsert(databaseJob(8, 'backup_database', 'shop_main'))
    renderPage({}, jobs)
    const main = await vi.waitFor(() => row('shop_main'))
    expect(within(main).getByText('Back up database…')).toBeInTheDocument()
    for (const name of ['Back up', 'Snapshot', 'Duplicate', 'Delete']) {
      expect(within(main).getByRole('button', { name })).toBeDisabled()
    }
    expect(within(row('shop_test')).getByRole('button', { name: 'Back up' })).toBeEnabled()

    await fireEvent.click(screen.getByRole('button', { name: 'Show progress' }))
    expect(within(dialog()).getByRole('heading', { name: 'Back up database' })).toBeInTheDocument()
    expect(within(dialog()).getByText('Starting…')).toBeInTheDocument()
    await fireEvent.keyDown(window, { key: 'Escape' })
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('explains an unusable PostgreSQL server and turns off creating', async () => {
    const check = envReport().checks.find((c) => c.id === 'postgresql')
    if (!check) throw new Error('no postgresql check')
    renderPage({
      'GET /projects/shop/databases': [
        200,
        databaseList([], {
          error: { ...check, status: 'error', summary: 'PostgreSQL is not reachable', details: ['/run/postgresql: no socket'] },
        }),
      ],
    })
    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('PostgreSQL is not reachable')
    expect(within(alert).getByRole('link', { name: 'Open the environment check' })).toHaveAttribute('href', '/environment')
    expect(screen.getByRole('button', { name: 'New database…' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Restore a backup…' })).toBeDisabled()
    expect(screen.queryByText(/No databases yet/)).toBeNull()
  })

  it('restores an uploaded file with a name taken from it', async () => {
    renderPage()
    await fireEvent.click(await screen.findByRole('button', { name: 'Restore a backup…' }))
    const file = new File(['PK'], 'shop prod 2026.zip', { type: 'application/zip' })
    await fireEvent.change(within(dialog()).getByLabelText('Backup file'), { target: { files: [file] } })
    expect(within(dialog()).getByLabelText('Database name')).toHaveValue('shop_prod_2026')
    expect(within(dialog()).getByLabelText(/Neutralize/)).toBeChecked()
    expect(within(dialog()).getByLabelText(/It is a copy/)).toBeChecked()
  })
})
