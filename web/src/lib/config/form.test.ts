import { describe, expect, it } from 'vitest'
import { ApiClient } from '../api/client'
import { configSchema as SCHEMA, projectOptions as saved, routedFetch } from '../../test-fixtures'
import { ConfigForm, MAX_IMPORT_BYTES } from './form.svelte'

function setup(putAnswer: [number, unknown?] = [200, saved()]) {
  const { fetcher, calls } = routedFetch({
    'GET /projects/shop/schema': [200, SCHEMA],
    'GET /projects/shop/options': [200, saved()],
    'PUT /projects/shop/options': putAnswer,
  })
  return { form: new ConfigForm(new ApiClient(fetcher), 'shop'), calls }
}

describe('ConfigForm', () => {
  it('loads the editable options and splits known, custom and secret ones', async () => {
    const { form } = setup()
    await form.load()
    expect(form.status).toBe('ready')
    expect(form.options.map((o) => o.key)).toEqual(['admin_passwd', 'db_password', 'db_host', 'log_level', 'limit_time_cpu', 'list_db'])
    expect(form.values).toEqual({ log_level: 'debug' })
    expect(form.customRows.map((r) => [r.key, r.value])).toEqual([['x_module', 'on']])
    expect(form.shownSections.map((s) => s.title)).toEqual([
      'Config file only',
      'Database related options',
      'Logging Configuration',
      'Advanced options',
    ])
    expect(form.dirty).toBe(false)
  })

  it('searches and filters to options with a value', async () => {
    const { form } = setup()
    await form.load()
    form.query = 'database HOST'
    expect(form.shownSections.flatMap((s) => s.options.map((o) => o.key))).toEqual(['db_host'])
    form.query = ''
    form.onlySet = true
    expect(form.shownSections.flatMap((s) => s.options.map((o) => o.key))).toEqual(['admin_passwd', 'log_level'])
    form.setSecret('admin_passwd', { action: 'remove' })
    expect(form.shownCount).toBe(1)
  })

  it('blocks saving while a value is invalid', async () => {
    const { form, calls } = setup()
    await form.load()
    form.setValue('limit_time_cpu', 'soon')
    expect(form.problems).toEqual({ limit_time_cpu: 'Enter a whole number' })
    expect(form.dirty).toBe(true)
    expect(form.canSave).toBe(false)
    expect(await form.save()).toBe(false)
    expect(calls.filter((c) => c.method === 'PUT')).toHaveLength(0)
    const row = form.addRow()
    row.key = 'http_port'
    expect(Object.values(form.rowProblems)).toEqual(['http_port is set by OdHive'])
    row.key = 'x_module'
    expect(Object.values(form.rowProblems)).toEqual(['x_module is set twice'])
  })

  it('sends every non-secret value and only the secrets that changed', async () => {
    const after = saved({
      options: { limit_time_cpu: '120', x_module: 'on', x_other: '1' },
      secrets_set: ['admin_passwd', 'db_password'],
    })
    const { form, calls } = setup([200, after])
    await form.load()
    form.setValue('log_level', '')
    form.setValue('limit_time_cpu', '120')
    const row = form.addRow()
    row.key = ' x_other '
    row.value = '1'
    form.addRow() // blank rows are ignored
    form.setSecret('db_password', { action: 'set', value: 'pw' })
    form.setSecret('smtp_password', { action: 'set', value: '' }) // typed nothing: unchanged
    expect(await form.save()).toBe(true)
    const put = calls.find((c) => c.method === 'PUT')
    expect(put?.body).toEqual({
      options: { limit_time_cpu: '120', x_module: 'on', x_other: '1' },
      secrets: { db_password: 'pw' },
    })
    expect(form.savedNow).toBe(true)
    expect(form.secretsSet).toEqual(['admin_passwd', 'db_password'])
    expect(form.customRows.map((r) => r.key)).toEqual(['x_module', ' x_other '])
  })

  it('removes a saved secret only when it is set', async () => {
    const { form, calls } = setup()
    await form.load()
    form.setSecret('admin_passwd', { action: 'remove' })
    form.setSecret('db_password', { action: 'remove' }) // not saved: nothing to remove
    expect(form.secretChanges).toEqual({ admin_passwd: null })
    await form.save()
    expect(calls.find((c) => c.method === 'PUT')?.body).toMatchObject({ secrets: { admin_passwd: null } })
  })

  it('discards changes back to the saved options', async () => {
    const { form } = setup()
    await form.load()
    form.setValue('db_host', 'pg')
    form.removeRow(form.customRows[0]?.id ?? -1)
    form.setSecret('admin_passwd', { action: 'remove' })
    expect(form.dirty).toBe(true)
    form.discard()
    expect(form.dirty).toBe(false)
    expect(form.values).toEqual({ log_level: 'debug' })
    expect(form.customRows.map((r) => r.key)).toEqual(['x_module'])
  })

  it('shows the server message when saving fails, and load errors', async () => {
    const { form } = setup([422, { error: 'invalid_option', message: 'invalid value for list_db: nope' }])
    await form.load()
    form.setValue('db_host', 'pg')
    expect(await form.save()).toBe(false)
    expect(form.saveError).toBe('invalid value for list_db: nope')
    expect(form.dirty).toBe(true)

    const { fetcher } = routedFetch({ 'GET /projects/shop/schema': [404, { error: 'unknown_project', message: 'no project' }] })
    const broken = new ConfigForm(new ApiClient(fetcher), 'shop')
    await broken.load()
    expect([broken.status, broken.loadError]).toEqual(['error', 'no project'])
  })

  it('imports a file into the form, refusing huge files and showing server errors', async () => {
    const { fetcher, calls } = routedFetch({
      'GET /projects/shop/schema': [200, SCHEMA],
      'GET /projects/shop/options': [200, saved()],
      'POST /projects/shop/options/import': [422, { error: 'bad_conf', message: 'no [options] section' }],
    })
    const form = new ConfigForm(new ApiClient(fetcher), 'shop')
    await form.load()
    const big = { size: MAX_IMPORT_BYTES + 1, text: () => Promise.resolve('') }
    expect(await form.importFile(big)).toBe(false)
    expect(form.importError).toMatch(/larger than 512 KiB/)
    expect(calls.some((c) => c.method === 'POST')).toBe(false)
    expect(await form.importFile({ size: 3, text: () => Promise.resolve('x=1') })).toBe(false)
    expect(form.importError).toBe('no [options] section')
    expect(form.dirty).toBe(false)

    form.applyImport({ options: { db_host: 'pg', x_new: '1' }, secrets: { admin_passwd: 'f' }, warnings: ['w'] })
    expect(form.values).toEqual({ db_host: 'pg' })
    expect(form.customRows.map((r) => r.key)).toEqual(['x_new'])
    expect(form.update).toEqual({ options: { db_host: 'pg', x_new: '1' }, secrets: { admin_passwd: 'f' } })
    expect(form.importNotes).toEqual(['w'])
  })
  it('reveals one saved secret on request and hides it on edit, save and discard', async () => {
    const { fetcher, calls } = routedFetch({
      'GET /projects/shop/schema': [200, SCHEMA],
      'GET /projects/shop/options': [200, saved()],
      'PUT /projects/shop/options': [200, saved()],
      'POST /projects/shop/options/reveal': [200, { key: 'admin_passwd', value: 's3cret' }],
    })
    const form = new ConfigForm(new ApiClient(fetcher), 'shop')
    await form.load()
    expect(form.revealed).toEqual({})
    expect(await form.reveal('db_password')).toBe(false) // nothing saved: not asked
    expect(calls.some((c) => c.method === 'POST')).toBe(false)

    expect(await form.reveal('admin_passwd')).toBe(true)
    expect(calls.at(-1)).toMatchObject({ method: 'POST', path: '/projects/shop/options/reveal', body: { key: 'admin_passwd' } })
    expect(form.revealed).toEqual({ admin_passwd: 's3cret' })
    expect(form.dirty).toBe(false)
    expect(form.update.secrets).toEqual({}) // a revealed secret is not sent back
    form.hide('admin_passwd')
    expect(form.revealed).toEqual({})

    await form.reveal('admin_passwd')
    form.setSecret('admin_passwd', { action: 'remove' })
    expect(form.revealed).toEqual({})
    form.setSecret('admin_passwd', { action: 'keep' })
    await form.reveal('admin_passwd')
    form.discard()
    expect(form.revealed).toEqual({})
    await form.reveal('admin_passwd')
    form.setValue('db_host', 'pg')
    await form.save()
    expect(form.revealed).toEqual({})
  })

  it('drops a reveal answered after an edit, and shows reveal errors', async () => {
    let answer: (r: Response) => void = () => {}
    const { fetcher } = routedFetch({
      'GET /projects/shop/schema': [200, SCHEMA],
      'GET /projects/shop/options': [200, saved()],
    })
    const form = new ConfigForm(
      new ApiClient(async (input, init) =>
        String(input).endsWith('/reveal') ? new Promise<Response>((resolve) => (answer = resolve)) : fetcher(input, init),
      ),
      'shop',
    )
    await form.load()
    const late = form.reveal('admin_passwd')
    expect(form.revealing).toBe('admin_passwd')
    form.setSecret('admin_passwd', { action: 'set', value: 'new' })
    answer(Response.json({ key: 'admin_passwd', value: 'old' }))
    expect(await late).toBe(false)
    expect(form.revealed).toEqual({})
    expect(form.revealing).toBe(null)

    form.setSecret('admin_passwd', { action: 'keep' })
    const failed = form.reveal('admin_passwd')
    answer(Response.json({ error: 'secret_not_set', message: 'admin_passwd has no saved value' }, { status: 404 }))
    expect(await failed).toBe(false)
    expect(form.revealErrors).toEqual({ admin_passwd: 'admin_passwd has no saved value' })
    form.hide('admin_passwd')
    expect(form.revealErrors).toEqual({})
  })
})
