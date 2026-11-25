import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import { ApiClient } from '../lib/api/client'
import { configSchema, projectOptions, routedFetch } from '../test-fixtures'
import ConfigPage from './ConfigPage.svelte'

function renderPage(put: [number, unknown?] = [200, projectOptions()]) {
  const { fetcher, calls } = routedFetch({
    'GET /projects/shop/schema': [200, configSchema],
    'GET /projects/shop/options': [200, projectOptions()],
    'PUT /projects/shop/options': put,
  })
  render(ConfigPage, { project: 'shop', api: new ApiClient(fetcher) })
  return { calls }
}

async function loaded(): Promise<void> {
  await screen.findByRole('searchbox', { name: 'Search options' })
}

describe('ConfigPage', () => {
  it('says when the bundled option list is shown, with the reason folded', async () => {
    const { fetcher } = routedFetch({
      'GET /projects/shop/schema': [200, { ...configSchema, source: 'bundled', reason: 'no venv yet' }],
      'GET /projects/shop/options': [200, projectOptions()],
    })
    render(ConfigPage, { project: 'shop', api: new ApiClient(fetcher) })
    const note = await screen.findByTestId('bundled-schema')
    expect(note).toHaveTextContent('bundled with OdHive for Odoo 19.0')
    expect(within(note).getByText('no venv yet')).not.toBeVisible()
  })


  it('renders a field per option with the widget of its type, help and default', async () => {
    renderPage()
    await loaded()
    const level = screen.getByLabelText('log_level')
    expect(level.tagName).toBe('SELECT')
    expect(level).toHaveValue('debug')
    expect(within(level).getAllByRole('option').map((o) => o.textContent)).toEqual(['Default (info)', 'info', 'debug', 'warn'])
    expect(level).toHaveAccessibleDescription('Default: info')

    const host = screen.getByLabelText('db_host')
    expect(host).toHaveValue('')
    expect(host).toHaveAccessibleDescription('specify the database host No default')
    expect(screen.getByLabelText('list_db').tagName).toBe('SELECT')
    expect(screen.getByLabelText('limit_time_cpu')).toHaveAttribute('inputmode', 'numeric')
    // Managed and command-line only options get no field.
    expect(screen.queryByLabelText('http_port')).toBeNull()
    expect(screen.queryByLabelText('save')).toBeNull()
    expect(screen.getByLabelText('Key of option 1')).toHaveValue('x_module')
  })

  it('never shows a saved secret; replacing it starts from an empty password field', async () => {
    renderPage()
    await loaded()
    expect(screen.queryByLabelText('admin_passwd')).toBeNull()
    await fireEvent.click(screen.getByRole('button', { name: 'Replace admin_passwd' }))
    const field = screen.getByLabelText('admin_passwd')
    expect(field).toHaveAttribute('type', 'password')
    expect(field).toHaveValue('')
    expect(field).toHaveFocus()
    // A secret without a saved value is a password field right away.
    expect(screen.getByLabelText('db_password')).toHaveAttribute('type', 'password')
  })

  it('shows problems while typing and resets to the default', async () => {
    renderPage()
    await loaded()
    const cpu = screen.getByLabelText('limit_time_cpu')
    await fireEvent.input(cpu, { target: { value: 'soon' } })
    expect(cpu).toHaveAttribute('aria-invalid', 'true')
    expect(cpu).toHaveAccessibleDescription('Enter a whole number Default: 60')
    expect(screen.getByRole('status')).toHaveTextContent('Unsaved changes.')
    await fireEvent.click(screen.getByRole('button', { name: 'Reset limit_time_cpu to default' }))
    expect(screen.getByLabelText('limit_time_cpu')).toHaveValue('')
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled()
  })

  it('searches across sections, which open while searching and fold otherwise', async () => {
    renderPage()
    await loaded()
    const toggle = screen.getByRole('button', { name: /Logging Configuration/ })
    await fireEvent.click(toggle)
    expect(toggle).toHaveAttribute('aria-expanded', 'false')
    expect(screen.queryByLabelText('log_level')).not.toBeVisible()

    await fireEvent.input(screen.getByRole('searchbox'), { target: { value: 'level' } })
    expect(screen.getByText('1 of 6 options')).toBeInTheDocument()
    expect(screen.getByLabelText('log_level')).toBeVisible()
    expect(screen.queryByLabelText('db_host')).toBeNull()
    await fireEvent.input(screen.getByRole('searchbox'), { target: { value: 'nothing-like-this' } })
    expect(screen.getByText('No option matches.')).toBeInTheDocument()
  })

  it('saves the changes and says when they apply', async () => {
    const { calls } = renderPage([200, projectOptions({ options: { log_level: 'warn', x_module: 'on' } })])
    await loaded()
    await fireEvent.change(screen.getByLabelText('log_level'), { target: { value: 'warn' } })
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    await vi.waitFor(() =>
      expect(screen.getByRole('status')).toHaveTextContent('Saved. Running instances use the new options after a restart.'),
    )
    expect(calls.find((c) => c.method === 'PUT')?.body).toEqual({
      options: { log_level: 'warn', x_module: 'on' },
      secrets: {},
    })
  })

  it('focuses the first invalid field instead of saving', async () => {
    const { calls } = renderPage()
    await loaded()
    await fireEvent.click(screen.getByRole('button', { name: 'Add option' }))
    const key = screen.getByLabelText('Key of option 2')
    expect(key).toHaveFocus()
    await fireEvent.input(key, { target: { value: 'Bad Key' } })
    await fireEvent.submit(screen.getByRole('button', { name: 'Save' }).closest('form') as HTMLFormElement)
    await vi.waitFor(() => expect(key).toHaveFocus())
    expect(screen.getByText('Lowercase letters, digits, _ . - only')).toBeInTheDocument()
    expect(calls.filter((c) => c.method === 'PUT')).toHaveLength(0)
  })
})
