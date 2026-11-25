import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import { ApiClient } from '../lib/api/client'
import { configSchema, projectOptions, routedFetch } from '../test-fixtures'
import ConfigPage from './ConfigPage.svelte'

const SAVED_CONF = '[options]\naddons_path = /src/addons\nlog_level = debug\nadmin_passwd = ********\n'
const PREVIEW = { text: SAVED_CONF.replace('debug', 'warn'), saved: SAVED_CONF }
const IMPORTED = {
  options: { log_level: 'warn', x_other: '2' },
  secrets: { db_password: 'from-file' },
  warnings: ['http_port = 8070: set per instance by OdHive, not imported'],
}

function renderPage(put: [number, unknown?] = [200, projectOptions()]) {
  const { fetcher, calls } = routedFetch({
    'GET /projects/shop/schema': [200, configSchema],
    'GET /projects/shop/options': [200, projectOptions()],
    'PUT /projects/shop/options': put,
    'POST /projects/shop/options/preview': [200, PREVIEW],
    'POST /projects/shop/options/import': [200, IMPORTED],
    'POST /projects/shop/options/reveal': [200, { key: 'admin_passwd', value: 's3cret-<b>' }],
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

  it('reveals a saved secret only on request, as text, and hides it again', async () => {
    const { calls } = renderPage()
    await loaded()
    const field = document.querySelector('[data-option="admin_passwd"]') as HTMLElement
    expect(field).not.toHaveTextContent('s3cret')
    expect(calls.some((c) => c.path.endsWith('/reveal'))).toBe(false)
    const reveal = screen.getByRole('button', { name: 'Reveal admin_passwd' })
    expect(reveal).toHaveAttribute('aria-pressed', 'false')
    await fireEvent.click(reveal)
    const hide = await screen.findByRole('button', { name: 'Hide admin_passwd' })
    expect(hide).toHaveAttribute('aria-pressed', 'true')
    expect(within(field).getByText('s3cret-<b>')).toBeInTheDocument()
    expect(field.querySelector('b')).toBeNull()
    expect(calls.filter((c) => c.path.endsWith('/reveal')).map((c) => c.body)).toEqual([{ key: 'admin_passwd' }])
    // The revealed value never goes into a save or preview body.
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled()
    await fireEvent.click(hide)
    expect(field).not.toHaveTextContent('s3cret')
    await fireEvent.click(screen.getByRole('button', { name: 'Reveal admin_passwd' }))
    await screen.findByText('s3cret-<b>')
    await fireEvent.click(screen.getByRole('button', { name: 'Replace admin_passwd' }))
    expect(field).not.toHaveTextContent('s3cret')
  })

  it('shows a typed secret as text on request', async () => {
    renderPage()
    await loaded()
    const field = screen.getByLabelText('db_password')
    await fireEvent.input(field, { target: { value: 'typed' } })
    const show = screen.getByRole('button', { name: 'Show typed db_password' })
    expect(show).toHaveAttribute('aria-pressed', 'false')
    await fireEvent.click(show)
    expect(field).toHaveAttribute('type', 'text')
    expect(field).toHaveValue('typed')
    await fireEvent.click(screen.getByRole('button', { name: 'Hide typed db_password' }))
    expect(field).toHaveAttribute('type', 'password')
  })

  it('shows problems while typing and resets to the default', async () => {
    renderPage()
    await loaded()
    const cpu = screen.getByLabelText('limit_time_cpu')
    await fireEvent.input(cpu, { target: { value: 'soon' } })
    expect(cpu).toHaveAttribute('aria-invalid', 'true')
    expect(cpu).toHaveAccessibleDescription('Enter a whole number Default: 60')
    expect(screen.getByText('Unsaved changes.')).toBeInTheDocument()
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
      expect(screen.getByText('Saved to odoo.conf. Running instances use the new options after a restart.')).toBeInTheDocument(),
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

  it('previews odoo.conf as the server renders it, with the changes against the saved file', async () => {
    const { calls } = renderPage()
    await loaded()
    const file = screen.getByRole('region', { name: 'odoo.conf preview' })
    await vi.waitFor(() => expect(file).toHaveTextContent('log_level = warn'))
    expect(screen.getByText(/\/home\/dev\/OdHive\/projects\/shop\/odoo\.conf/)).toBeInTheDocument()
    await fireEvent.click(screen.getByRole('button', { name: 'Changes (2)' }))
    const changes = screen.getByRole('region', { name: 'Changes against the saved odoo.conf' })
    expect([...changes.querySelectorAll('[data-kind]')].map((l) => [l.getAttribute('data-kind'), l.textContent])).toEqual([
      ['same', ' [options]'],
      ['same', ' addons_path = /src/addons'],
      ['del', '-removed: log_level = debug'],
      ['add', '+added: log_level = warn'],
      ['same', ' admin_passwd = ********'],
    ])
    expect(calls.find((c) => c.path === '/projects/shop/options/preview')?.body).toEqual({
      options: { log_level: 'debug', x_module: 'on' },
      secrets: {},
    })
  })

  it('holds the preview while a value is invalid', async () => {
    const { calls } = renderPage()
    await loaded()
    await vi.waitFor(() => expect(calls.some((c) => c.path.endsWith('/preview'))).toBe(true))
    const before = calls.length
    await fireEvent.input(screen.getByLabelText('limit_time_cpu'), { target: { value: 'soon' } })
    expect(screen.getByText('The preview waits until the marked options are fixed.')).toBeInTheDocument()
    await new Promise((resolve) => setTimeout(resolve, 400))
    expect(calls.length).toBe(before)
  })

  it('downloads the preview as odoo.conf', async () => {
    const created: Blob[] = []
    const urls = vi.spyOn(URL, 'createObjectURL').mockImplementation((blob) => {
      created.push(blob as Blob)
      return 'blob:x'
    })
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function (this: HTMLAnchorElement) {
      expect(this.download).toBe('odoo.conf')
    })
    try {
      renderPage()
      await loaded()
      await vi.waitFor(() => expect(screen.getByRole('button', { name: 'Download' })).toBeEnabled())
      await fireEvent.click(screen.getByRole('button', { name: 'Download' }))
      expect(click).toHaveBeenCalledOnce()
      expect(await created[0]?.text()).toBe(PREVIEW.text)
    } finally {
      urls.mockRestore()
      click.mockRestore()
    }
  })

  it('imports an odoo.conf into the form for review', async () => {
    const { calls } = renderPage()
    await loaded()
    const input = screen.getByTestId('import-file') as HTMLInputElement
    const file = new File(['[options]\nlog_level = warn\n'], 'odoo.conf', { type: 'text/plain' })
    await fireEvent.change(input, { target: { files: [file] } })
    const notes = await screen.findByTestId('import-notes')
    expect(notes).toHaveTextContent('http_port = 8070: set per instance by OdHive, not imported')
    expect(calls.find((c) => c.path.endsWith('/import'))?.body).toEqual({ text: '[options]\nlog_level = warn\n' })
    expect(screen.getByLabelText('log_level')).toHaveValue('warn')
    expect(screen.getByLabelText('Key of option 1')).toHaveValue('x_other')
    // The file's secret is set but not shown as text.
    expect(screen.getByLabelText('db_password')).toHaveAttribute('type', 'password')
    await vi.waitFor(() => expect(screen.getByRole('button', { name: /^Changes/ })).toHaveAttribute('aria-pressed', 'true'))
    expect(screen.getByText('Unsaved changes.')).toBeInTheDocument()
  })
})
