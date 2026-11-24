import { existsSync, readFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'
import type { APIRequestContext } from '@playwright/test'
import type { E2eState } from './env'
import { expect, seriousViolations, test } from './fixtures'

const XSS = '<script>alert(1)</script><img src=x onerror="alert(2)">'
const SETTINGS = '/api/v1/projects/shop/instances/main/settings'

function api(request: APIRequestContext, e2e: E2eState) {
  const headers = { Authorization: `Bearer ${e2e.token}`, 'X-OdHive': '1' }
  return {
    get: (path: string) => request.get(`${e2e.baseURL}${path}`, { headers }),
    post: (path: string) => request.post(`${e2e.baseURL}${path}`, { headers }),
    put: (path: string, data: unknown) => request.put(`${e2e.baseURL}${path}`, { headers, data }),
  }
}

test('the log viewer streams entries as text and follows the end', async ({ page, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const saved = (await (await client.get(SETTINGS)).json()) as { settings: Record<string, unknown> }
  const put = await client.put(SETTINGS, {
    ...saved.settings,
    extra_args: ['--log-env', 'ODHIVE_E2E_XSS', '--simulate', 'traceback', '--flood', '3000'],
    env: { ODHIVE_E2E_XSS: XSS },
  })
  expect(put.status(), await put.text()).toBe(200)

  let dialogs = 0
  page.on('dialog', (dialog) => {
    dialogs += 1
    void dialog.dismiss()
  })

  try {
    await signIn()
    await page.goto('/projects/shop/instances/main')
    const log = page.getByRole('region', { name: 'Log of shop/main' })
    await expect(log).toBeVisible()
    await expect(log.getByText('No log yet.', { exact: false })).toBeVisible()

    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)

    // Following: the newest flood line comes into view on its own.
    await expect(log.getByText(/flood line 3000\b/)).toBeVisible({ timeout: 30_000 })
    await expect(page.getByRole('button', { name: /Jump to latest/ })).toBeHidden()

    // The traceback (an ERROR, logged last) and the level colours pass
    // contrast in both themes.
    await expect(log.getByText(/ValueError: Expected singleton/)).toBeVisible()
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      await expect(page.locator('html')).toHaveAttribute('data-theme', scheme)
      expect(await seriousViolations(page), scheme).toEqual([])
    }

    // Markup in a log line is text: no script ran, no element was made.
    await log.evaluate((element) => {
      element.scrollTop = 0
    })
    await expect(log.getByText(`env ODHIVE_E2E_XSS=${XSS}`)).toBeVisible()
    expect(await log.locator('script, img').count()).toBe(0)
    expect(dialogs).toBe(0)

    // Scrolled up: paused, until "Jump to latest".
    const jump = page.getByRole('button', { name: /Jump to latest/ })
    await expect(jump).toBeVisible()
    await jump.click()
    await expect(jump).toBeHidden()
    await expect(log.getByText(/flood line 3000\b/)).toBeVisible()

    // Level chips: without INFO only the traceback (ERROR) is left.
    const info = page.getByRole('button', { name: 'INFO', exact: true })
    await info.click()
    await expect(info).toHaveAttribute('aria-pressed', 'false')
    await expect(log.getByText(/flood line/)).toHaveCount(0)
    await expect(log.getByText(/ValueError: Expected singleton/)).toBeVisible()
    // Logger filter: hiding its logger leaves nothing.
    const loggers = page.getByRole('textbox', { name: 'Logger' })
    await loggers.fill('-odoo.http')
    await expect(log.getByText('No entries match the level and logger filters.')).toBeVisible()
    await loggers.fill('')
    await info.click()
    await expect(log.getByText(/flood line 3000\b/)).toBeVisible()

    // Regex search: highlights, counts and steps through the matches.
    await page.getByRole('button', { name: 'Regular expression' }).click()
    const search = page.getByRole('searchbox', { name: 'Search the log' })
    await search.fill(String.raw`flood line 12\d\b`)
    const status = page.getByTestId('log-search-status')
    await expect(status).toHaveText('10 matches')
    await search.press('Enter')
    await expect(status).toHaveText('1 of 10')
    const current = log.locator('[data-current]')
    await expect(current.locator('mark')).toHaveText('flood line 120')
    await expect(current).toBeInViewport()
    await search.press('Enter')
    await expect(current.locator('mark')).toHaveText('flood line 121')
    await search.press('Shift+Enter')
    await expect(status).toHaveText('1 of 10')
    // Highlights pass contrast in both themes.
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), `search, ${scheme}`).toEqual([])
    }
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})

test('tracebacks fold, copy exactly and open file:line in the editor', async ({ page, context, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const saved = (await (await client.get(SETTINGS)).json()) as { settings: Record<string, unknown> }
  const put = await client.put(SETTINGS, { ...saved.settings, extra_args: ['--simulate', 'traceback'] })
  expect(put.status(), await put.text()).toBe(200)
  await context.grantPermissions(['clipboard-read', 'clipboard-write'])
  rmSync(e2e.editorLog, { force: true })

  try {
    await signIn()
    await page.goto('/projects/shop/instances/main')
    const log = page.getByRole('region', { name: 'Log of shop/main' })
    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)

    // Folded to one line: the message, the exception and what is hidden.
    const row = log.locator('[data-folded]')
    await expect(row).toContainText(
      'Exception during request handling. · ValueError: Expected singleton: res.partner(1, 2) · 8 more lines',
      { timeout: 30_000 },
    )
    await expect(log.getByText('partner.ensure_one()')).toHaveCount(0)

    // Copy traceback: exactly the logged traceback.
    await row.getByRole('button', { name: 'Copy traceback' }).click()
    await expect(row.getByRole('button', { name: 'Copied' })).toBeVisible()
    const copied = await page.evaluate(() => navigator.clipboard.readText())
    expect(copied.split('\n')[0]).toBe('Traceback (most recent call last):')
    expect(copied.split('\n')).toHaveLength(8)
    expect(copied).toMatch(/File ".*\/odoo-bin", line \d+, in <module>\n {4}sys\.exit\(main\(sys\.argv\[1:\]\)\)\n/)
    expect(copied.endsWith('\nValueError: Expected singleton: res.partner(1, 2)')).toBe(true)

    // Unfolded: every frame, paths as links.
    await log.getByRole('button', { name: 'Unfold traceback' }).click()
    await expect(log.getByText('partner.ensure_one()', { exact: false })).toBeVisible()
    const oddoBin = log.locator('button[data-path$="/odoo-src/odoo-bin"]')
    await expect(oddoBin).toBeVisible()
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), `traceback, ${scheme}`).toEqual([])
    }

    // A file of the Odoo source opens in the (fake) editor at its line.
    const line = await oddoBin.getAttribute('data-line')
    await oddoBin.click()
    await expect.poll(() => (existsSync(e2e.editorLog) ? readFileSync(e2e.editorLog, 'utf8') : '')).toBe(
      `${join(e2e.home, 'odoo-src/odoo-bin')}:${line}\n`,
    )
    // A path outside the project and its sources is refused.
    rmSync(e2e.editorLog)
    await log.locator('button[data-path="/opt/odoo/odoo/http.py"]').click()
    const toast = page.getByRole('alert').filter({ hasText: 'Could not open http.py:2104' })
    await expect(toast).toContainText('outside the project and its Odoo sources')
    expect(existsSync(e2e.editorLog)).toBe(false)
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})
