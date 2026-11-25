import { readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'
import type { APIRequestContext } from '@playwright/test'
import type { E2eState } from './env'
import { expect, seriousViolations, test } from './fixtures'

const OPTIONS = '/api/v1/projects/shop/options'
const SECRET = 'e2e-master-password-7c1f'

function api(request: APIRequestContext, e2e: E2eState) {
  const headers = { Authorization: `Bearer ${e2e.token}`, 'X-OdHive': '1' }
  return {
    get: (path: string) => request.get(`${e2e.baseURL}${path}`, { headers }),
    put: (path: string, data: unknown) => request.put(`${e2e.baseURL}${path}`, { headers, data }),
  }
}

test('the config builder edits options from the schema and keeps secrets hidden', async ({
  page,
  signIn,
  request,
  e2e,
}) => {
  const client = api(request, e2e)
  const project = (await (await client.get('/api/v1/projects/shop')).json()) as { path: string }
  try {
    await signIn()
    await page.goto('/projects/shop')
    await page.getByRole('link', { name: 'Odoo options' }).click()
    await expect(page).toHaveURL(/\/projects\/shop\/config$/)
    await expect(page.getByRole('heading', { level: 1, name: 'Odoo options' })).toBeVisible()

    // Search narrows the form to matching options, in their sections.
    const search = page.getByRole('searchbox', { name: 'Search options' })
    await search.fill('log level')
    const level = page.getByLabel('log_level', { exact: true })
    await expect(level).toBeVisible()
    await expect(page.getByLabel('db_host', { exact: true })).toHaveCount(0)
    await level.selectOption('debug')

    // A bad value is flagged while typing and blocks saving.
    await search.fill('limit_time_cpu')
    const cpu = page.getByLabel('limit_time_cpu', { exact: true })
    await cpu.fill('soon')
    await expect(cpu).toHaveAttribute('aria-invalid', 'true')
    await expect(page.getByText('Enter a whole number')).toBeVisible()
    await cpu.fill('120')
    await expect(cpu).toHaveAttribute('aria-invalid', 'false')

    await search.fill('admin_passwd')
    await page.getByLabel('admin_passwd', { exact: true }).fill(SECRET)

    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      await expect(page.locator('html')).toHaveAttribute('data-theme', scheme)
      expect(await seriousViolations(page), scheme).toEqual([])
    }

    await page.getByRole('button', { name: 'Save' }).click()
    await expect(page.getByText('Saved to odoo.conf.')).toBeVisible()

    const toml = readFileSync(join(project.path, 'odhive.toml'), 'utf8')
    expect(toml).toContain('log_level = "debug"')
    expect(toml).toContain('limit_time_cpu = "120"')
    expect(toml).toContain(SECRET)

    // After a reload the values are back, the secret only as "set".
    await page.reload()
    await page.getByRole('searchbox', { name: 'Search options' }).fill('admin_passwd')
    await expect(page.getByRole('button', { name: 'Replace admin_passwd' })).toBeVisible()
    await expect(page.getByLabel('admin_passwd', { exact: true })).toHaveCount(0)
    expect(await page.content()).not.toContain(SECRET)

    // Reveal shows the saved value in its field only (the preview stays masked); Hide removes it.
    await page.getByRole('button', { name: 'Reveal admin_passwd' }).click()
    const field = page.locator('[data-option="admin_passwd"]')
    await expect(field.getByText(SECRET)).toBeVisible()
    const preview = page.getByRole('region', { name: 'odoo.conf preview' })
    await expect(preview).toContainText('admin_passwd = ********')
    await expect(preview).not.toContainText(SECRET)
    await page.getByRole('button', { name: 'Hide admin_passwd' }).click()
    expect(await page.content()).not.toContain(SECRET)

    const read = await (await client.get(OPTIONS)).text()
    expect(read).not.toContain(SECRET)
    expect(JSON.parse(read)).toMatchObject({
      options: { log_level: 'debug', limit_time_cpu: '120' },
      secrets_set: ['admin_passwd'],
    })
  } finally {
    const reset = await client.put(OPTIONS, { options: {}, secrets: { admin_passwd: null } })
    expect(reset.status(), await reset.text()).toBe(200)
  }
})

test('editing an option updates the preview, and saving writes that odoo.conf', async ({ page, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const project = (await (await client.get('/api/v1/projects/shop')).json()) as { path: string }
  const confPath = join(project.path, 'odoo.conf')
  try {
    await signIn()
    await page.goto('/projects/shop/config')
    const preview = page.getByRole('region', { name: 'odoo.conf preview' })
    await expect(preview).toContainText('addons_path = ')
    await expect(preview).not.toContainText('log_level')

    await page.getByRole('searchbox', { name: 'Search options' }).fill('log level')
    await page.getByLabel('log_level', { exact: true }).selectOption('warn')
    await expect(preview).toContainText('log_level = warn')
    await page.getByRole('button', { name: 'Changes (1)' }).click()
    const changes = page.getByRole('region', { name: 'Changes against the saved odoo.conf' })
    await expect(changes.locator('[data-kind="add"]')).toHaveText(['+added: log_level = warn'])

    // Import: the file's options land in the form, the rest is reported.
    await page.getByTestId('import-file').setInputFiles({
      name: 'odoo.conf',
      mimeType: 'text/plain',
      buffer: Buffer.from('[options]\nlog_level = warn\nlimit_time_cpu = 90\nhttp_port = 8070\n'),
    })
    await expect(page.getByTestId('import-notes')).toContainText('http_port = 8070')
    await page.getByRole('button', { name: 'Preview', exact: true }).click()
    await expect(preview).toContainText('limit_time_cpu = 90')

    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      await expect(page.locator('html')).toHaveAttribute('data-theme', scheme)
      expect(await seriousViolations(page), scheme).toEqual([])
    }

    // At phone width the long preview lines scroll inside the panel, not the page.
    await page.setViewportSize({ width: 360, height: 800 })
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(360)
    await page.setViewportSize({ width: 1280, height: 720 })

    await page.getByRole('button', { name: 'Save' }).click()
    await expect(page.getByText('Saved to odoo.conf.')).toBeVisible()
    // The file on disk is the preview (no secrets set, so nothing masked).
    expect(readFileSync(confPath, 'utf8')).toBe(`${(await preview.textContent()) ?? ''}`)
    expect(statSync(confPath).mode & 0o777).toBe(0o600)
    await page.getByRole('button', { name: /^Changes/ }).click()
    await expect(page.getByText('No changes against the saved options.')).toBeVisible()
  } finally {
    const reset = await client.put(OPTIONS, { options: {} })
    expect(reset.status(), await reset.text()).toBe(200)
  }
})


test('markup in option values and imported files renders as text', async ({ page, signIn, request, e2e }) => {
  const XSS = '<script>alert(1)</script><img src=x onerror="alert(2)">'
  const client = api(request, e2e)
  let dialogs = 0
  page.on('dialog', (dialog) => {
    dialogs += 1
    void dialog.dismiss()
  })
  try {
    await signIn()
    await page.goto('/projects/shop/config')
    const main = page.locator('main')
    const preview = page.getByRole('region', { name: 'odoo.conf preview' })
    await expect(preview).toContainText('addons_path = ')

    // A typed value shows up in the preview and the diff as text.
    await page.getByRole('searchbox', { name: 'Search options' }).fill('dbfilter')
    await page.getByLabel('dbfilter', { exact: true }).fill(XSS)
    await expect(preview).toContainText(`dbfilter = ${XSS}`)
    await page.getByRole('button', { name: 'Changes (1)' }).click()
    const changes = page.getByRole('region', { name: 'Changes against the saved odoo.conf' })
    await expect(changes.locator('[data-kind="add"]')).toHaveText([`+added: dbfilter = ${XSS}`])
    await page.getByRole('button', { name: 'Preview', exact: true }).click()

    // An imported file: markup in a custom option and in a section name (reported in the notes).
    await page.getByTestId('import-file').setInputFiles({
      name: 'odoo.conf',
      mimeType: 'text/plain',
      buffer: Buffer.from(`[options]\ndbfilter = ${XSS}\nx_markup = ${XSS}\n\n[<img src=x onerror=alert(3)>]\nkey = 1\n`),
    })
    await expect(page.getByTestId('import-notes')).toContainText('section [<img src=x onerror=alert(3)>]')
    await expect(changes).toContainText(`added: x_markup = ${XSS}`)
    await page.getByRole('button', { name: 'Preview', exact: true }).click()
    await expect(preview).toContainText(`x_markup = ${XSS}`)
    expect(await main.locator('script, img').count()).toBe(0)

    await page.getByRole('button', { name: 'Save' }).click()
    await expect(page.getByText('Saved to odoo.conf.')).toBeVisible()

    // Read back from the server after a reload: still text, in the field and the preview.
    await page.reload()
    await expect(preview).toContainText(`dbfilter = ${XSS}`)
    await page.getByRole('searchbox', { name: 'Search options' }).fill('dbfilter')
    await expect(page.getByLabel('dbfilter', { exact: true })).toHaveValue(XSS)

    expect(await main.locator('script, img').count()).toBe(0)
    expect(dialogs).toBe(0)
  } finally {
    const reset = await client.put(OPTIONS, { options: {} })
    expect(reset.status(), await reset.text()).toBe(200)
  }
})
