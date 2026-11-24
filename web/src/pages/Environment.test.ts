import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ApiClient } from '../lib/api/client'
import { EnvironmentStore } from '../lib/environment.svelte'
import { envReport, routedFetch } from '../test-fixtures'
import Environment from './Environment.svelte'

const BROKEN = envReport({
  postgresql: {
    status: 'error',
    summary: 'PostgreSQL is not reachable',
    details: ['localhost:5432: connection refused'],
    fix: {
      text: 'Install PostgreSQL (it starts on its own) and create a role for your user.',
      commands: ['sudo apt install postgresql', 'sudo -u postgres createuser --createdb dev'],
      url: 'https://www.postgresql.org/download/',
    },
  },
  git: { status: 'ok', version: '2.47.1', location: '/usr/bin/git' },
  uv: { status: 'info', summary: 'Not installed: venvs and requirements use Python’s venv and pip' },
})

function renderPage(report = BROKEN) {
  const { fetcher, calls } = routedFetch({ 'GET /environment': [200, report] })
  const environment = new EnvironmentStore(new ApiClient(fetcher))
  render(Environment, { environment })
  return { calls, environment }
}

/** The verdict, also announced to screen readers. */
async function announced(text: string): Promise<void> {
  await vi.waitFor(() => expect(screen.getByRole('status')).toHaveTextContent(text))
  expect(screen.getAllByText(text)).toHaveLength(2)
}

afterEach(() => vi.unstubAllGlobals())

describe('Environment', () => {
  it('runs the check on open and shows each result with its fix', async () => {
    const { calls } = renderPage()
    await announced('1 problem needs fixing before Odoo can run.')
    expect(calls).toHaveLength(1)
    expect(screen.getByText(/Commands are for apt; OdHive never runs them/)).toBeInTheDocument()

    const pg = screen.getByRole('article', { name: /PostgreSQL/ })
    expect(pg).toHaveAttribute('data-status', 'error')
    expect(within(pg).getByText('localhost:5432: connection refused')).toBeInTheDocument()
    const commands = within(pg).getByRole('list', { name: 'Commands for apt' })
    expect(within(commands).getAllByRole('listitem').map((li) => li.querySelector('code')?.textContent)).toEqual([
      'sudo apt install postgresql',
      'sudo -u postgres createuser --createdb dev',
    ])
    const more = within(pg).getByRole('link', { name: /Read more/ })
    expect(more).toHaveAttribute('href', 'https://www.postgresql.org/download/')
    expect(more).toHaveAttribute('rel', 'noopener noreferrer')

    const git = screen.getByRole('article', { name: /git/ })
    expect(within(git).getByText('2.47.1')).toBeInTheDocument()
    expect(within(git).getByText('/usr/bin/git')).toBeInTheDocument()
    expect(within(git).queryByText(/How to fix/)).toBeNull()
  })

  it('copies a command', async () => {
    const writeText = vi.fn(async () => {})
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    renderPage()
    const button = await screen.findByRole('button', { name: 'Copy command: sudo apt install postgresql' })
    await fireEvent.click(button)
    expect(writeText).toHaveBeenCalledWith('sudo apt install postgresql')
    expect(await within(button).findByText('Copied')).toBeInTheDocument()
  })

  it('checks again on request and says when all is well', async () => {
    const { calls } = renderPage(envReport({ uv: { status: 'info' } }))
    await announced('Everything Odoo needs is here.')
    await fireEvent.click(screen.getByRole('button', { name: 'Check again' }))
    expect(calls).toHaveLength(2)
  })

  it('reports a failed check', async () => {
    const { fetcher } = routedFetch({ 'GET /environment': [500, { error: 'internal', message: 'config unreadable' }] })
    render(Environment, { environment: new EnvironmentStore(new ApiClient(fetcher)) })
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not run the check: config unreadable')
  })
})
