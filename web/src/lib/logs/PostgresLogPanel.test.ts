import { fireEvent, render, screen } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import type { PostgresLog } from '../api'
import { ApiRequestError } from '../api/client'
import PostgresLogPanel from './PostgresLogPanel.svelte'
import { record } from './test-fixtures'

const FOUND = '/var/log/postgresql/postgresql-16-main.log'

function log(extra: Partial<PostgresLog> = {}): PostgresLog {
  return {
    path: FOUND,
    configured: null,
    found: [FOUND, '/var/log/postgresql/postgresql-15-main.log'],
    status: 'ok',
    message: null,
    hint: null,
    stream: 1,
    records: [],
    skipped: 0,
    next: 1,
    ...extra,
  }
}

function setup(first: PostgresLog, put: () => Promise<PostgresLog> = () => Promise.resolve(log())) {
  const api = {
    postgresLog: vi.fn(() => Promise.resolve(first)),
    putPostgresLog: vi.fn(put),
  }
  render(PostgresLogPanel, { project: 'shop', api, interval: 60_000 })
  return api
}

describe('PostgresLogPanel', () => {
  it('shows the file found and its entries', async () => {
    const api = setup(log({ records: [record(1, 'database system is ready to accept connections', { logger: 'postgresql' })], next: 2 }))
    expect(await screen.findByTestId('postgres-log-path')).toHaveTextContent(FOUND)
    expect(screen.getByText('(found)')).toBeInTheDocument()
    expect(await screen.findByText('database system is ready to accept connections')).toBeInTheDocument()
    expect(api.postgresLog).toHaveBeenCalledWith('shop', { limit: 1000 })
    // No Odoo/SQL chips on the server log.
    expect(screen.queryByRole('group', { name: 'Sources' })).not.toBeInTheDocument()
  })

  it('explains a log it may not read', async () => {
    setup(
      log({
        path: null,
        status: 'permission_denied',
        message: `cannot read ${FOUND}: permission denied`,
        hint: 'can be read by the group adm',
      }),
    )
    expect(await screen.findByText(/permission denied/)).toBeInTheDocument()
    expect(screen.getByText(/group adm/)).toBeInTheDocument()
  })

  it('saves another file and reads again', async () => {
    const api = setup(log())
    await fireEvent.click(await screen.findByRole('button', { name: 'Change…' }))
    const select = screen.getByLabelText('Log file')
    await fireEvent.change(select, { target: { value: 'other' } })
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    expect(screen.getByRole('alert')).toHaveTextContent('absolute path')
    expect(api.putPostgresLog).not.toHaveBeenCalled()

    await fireEvent.input(screen.getByLabelText('Path'), { target: { value: ' /srv/pg/main.log ' } })
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    await vi.waitFor(() => expect(api.putPostgresLog).toHaveBeenCalledWith('shop', { path: '/srv/pg/main.log' }))
    await vi.waitFor(() => expect(api.postgresLog).toHaveBeenCalledTimes(2))
    expect(screen.queryByRole('form', { name: 'PostgreSQL log file' })).not.toBeInTheDocument()
  })

  it('shows why a file was refused and goes back to automatic', async () => {
    const api = setup(log({ configured: '/srv/pg/main.log', path: '/srv/pg/main.log' }), () =>
      Promise.reject(new ApiRequestError(422, 'bad_log_path', 'no file at /x.log')),
    )
    await fireEvent.click(await screen.findByRole('button', { name: 'Change…' }))
    expect(screen.getByLabelText('Path')).toHaveValue('/srv/pg/main.log')
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('no file at /x.log')

    api.putPostgresLog.mockImplementation(() => Promise.resolve(log()))
    await fireEvent.change(screen.getByLabelText('Log file'), { target: { value: '' } })
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    await vi.waitFor(() => expect(api.putPostgresLog).toHaveBeenLastCalledWith('shop', { path: null }))
  })
})
