import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import type { Download, Page } from '@playwright/test'
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
    // Logger filter: hiding its logger leaves only OdHive's start event.
    const loggers = page.getByRole('textbox', { name: 'Logger' })
    await loggers.fill('-odoo.http')
    await expect(log.getByText(/ValueError: Expected singleton/)).toHaveCount(0)
    await expect(log.locator('[data-row-id]')).toHaveCount(1)
    await expect(log.locator('[data-event="started"]')).toBeVisible()
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

test('a Smart Reload restart shows inline as an event', async ({ page, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const project = (await (await client.get('/api/v1/projects/shop')).json()) as {
    path: string
    instances: { id: string; live: { reload?: { watching: boolean } } | null }[]
  }
  // A module that exists before the start (a new one is only reported).
  const module = join(project.path, 'addons/sale_x')
  mkdirSync(join(module, 'models'), { recursive: true })
  writeFileSync(join(module, '__manifest__.py'), "{'name': 'Sale X'}\n")
  writeFileSync(join(module, 'models/order.py'), '# v1\n')

  try {
    await signIn()
    await page.goto('/projects/shop/instances/main')
    const log = page.getByRole('region', { name: 'Log of shop/main' })
    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)
    await expect(log.locator('[data-event="started"]')).toContainText(/START +▶ started \(pid \d+\)/, {
      timeout: 30_000,
    })
    await expect
      .poll(
        async () => {
          const now = (await (await client.get('/api/v1/projects/shop')).json()) as typeof project
          return now.instances.find((i) => i.id === 'main')?.live?.reload?.watching ?? false
        },
        { timeout: 15_000 },
      )
      .toBe(true)

    writeFileSync(join(module, 'models/order.py'), '# v2\n')
    const reload = log.locator('[data-event="reload"]')
    await expect(reload).toContainText('RELOAD ↻ restarted: sale_x/models/order.py changed', { timeout: 30_000 })
    // The restarted process starts after it.
    await expect(log.locator('[data-event="started"]')).toHaveCount(2, { timeout: 30_000 })

    // Kept with INFO off (they are context), and readable in both themes.
    await page.getByRole('button', { name: 'INFO', exact: true }).click()
    await expect(reload).toBeVisible()
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), `events, ${scheme}`).toEqual([])
    }
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    rmSync(module, { recursive: true, force: true })
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

test('log text selects and copies natively, and as whole entries', async ({ page, context, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const saved = (await (await client.get(SETTINGS)).json()) as { settings: Record<string, unknown> }
  const put = await client.put(SETTINGS, { ...saved.settings, extra_args: ['--simulate', 'traceback'] })
  expect(put.status(), await put.text()).toBe(200)
  await context.grantPermissions(['clipboard-read', 'clipboard-write'])

  try {
    await signIn()
    await page.goto('/projects/shop/instances/main')
    const log = page.getByRole('region', { name: 'Log of shop/main' })
    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)
    const folded = log.locator('[data-folded]')
    await expect(folded).toBeVisible({ timeout: 30_000 })
    const copySelection = page.getByRole('button', { name: 'Copy selection' })
    await expect(copySelection).toBeDisabled()
    await expect(page.getByRole('button', { name: /Jump to latest/ })).toBeHidden()

    // Drag from the start of the first row to the end of the folded traceback.
    const first = log.locator('[data-row-id]').first()
    const from = await first.boundingBox()
    const to = await folded.boundingBox()
    if (!from || !to) throw new Error('rows not laid out')
    await page.mouse.move(from.x + 2, from.y + from.height / 2)
    await page.mouse.down()
    await page.mouse.move(to.x + to.width - 2, to.y + to.height / 2, { steps: 8 })
    await page.mouse.up()
    const rows = await log.locator('[data-row-id]').evaluateAll((elements) => {
      const selection = document.getSelection()
      return elements.filter((element) => selection?.containsNode(element, true)).length
    })
    expect(rows).toBeGreaterThan(1)
    // Selecting pauses follow, so new lines don't scroll the selection away.
    await expect(page.getByRole('button', { name: /Jump to latest/ })).toBeVisible()

    // Ctrl/Cmd+C copies what is shown, one row per line, without the buttons.
    await page.keyboard.press('ControlOrMeta+C')
    const shown = await page.evaluate(() => navigator.clipboard.readText())
    expect(shown.trim().split('\n')).toHaveLength(rows)
    expect(shown).toContain('Exception during request handling. · ValueError: Expected singleton')
    expect(shown).not.toContain('Copy traceback')

    // Copy selection: the same entries as Odoo logged them, tracebacks in full.
    await expect(copySelection).toBeEnabled()
    await copySelection.click()
    await expect(page.getByRole('button', { name: /^Copied \d+ entr/ })).toBeVisible()
    const whole = await page.evaluate(() => navigator.clipboard.readText())
    // OdHive's own marker lines have no header and stay as they are.
    expect(whole).toMatch(/^----- odhive: started pid \d+ -----$/m)
    expect(whole).toMatch(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d{3} \d+ INFO \? odoo: Odoo version /m)
    expect(whole).toMatch(/ ERROR \S+ \S+: Exception during request handling\.\nTraceback \(most recent call last\):\n/)
    expect(whole.endsWith('ValueError: Expected singleton: res.partner(1, 2)')).toBe(true)

    // Copy visible: every entry on screen.
    await page.getByRole('button', { name: 'Copy visible' }).click()
    const visible = await page.evaluate(() => navigator.clipboard.readText())
    expect(visible).toContain(whole.split('\n')[0] ?? '')
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})

test('the split view follows logs side by side on one subscription', async ({ page, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const saved = (await (await client.get(SETTINGS)).json()) as { settings: Record<string, unknown> }
  const put = await client.put(SETTINGS, { ...saved.settings, extra_args: ['--flood', '500'] })
  expect(put.status(), await put.text()).toBe(200)

  const subscribes: string[] = []
  page.on('websocket', (socket) => {
    socket.on('framesent', ({ payload }) => {
      if (typeof payload === 'string' && payload.includes('"subscribe_logs"')) subscribes.push(payload)
    })
  })

  try {
    await page.setViewportSize({ width: 1440, height: 900 })
    await signIn()
    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)

    await page.goto('/projects/shop/instances/main')
    await expect(page.getByRole('region', { name: 'Log of shop/main' }).getByText(/flood line 500\b/)).toBeVisible({
      timeout: 30_000,
    })
    await page.getByRole('link', { name: 'Split view' }).click()
    await expect(page).toHaveURL(/\/logs\/shop\/main$/)
    await expect(page.getByRole('heading', { level: 1, name: 'Logs' })).toBeVisible()

    // A second pane of the same instance shares the stream: its rows are
    // there at once and nothing is subscribed again.
    await page.getByRole('combobox', { name: 'Instance to add' }).selectOption('shop/main')
    await page.getByRole('button', { name: 'Add log' }).click()
    await expect(page).toHaveURL(/\/logs\/shop\/main\/shop\/main$/)
    const panes = page.getByRole('region', { name: 'shop/main', exact: true })
    await expect(panes).toHaveCount(2)
    const [left, right] = [panes.nth(0), panes.nth(1)]
    await expect(left.getByText(/flood line 500\b/)).toBeVisible()
    await expect(right.getByText(/flood line 500\b/)).toBeVisible()
    expect(subscribes).toHaveLength(1)

    // Side by side at 1440 px, each with its own filters.
    const [a, b] = [await left.boundingBox(), await right.boundingBox()]
    expect(a && b && b.x > a.x + a.width - 1 && Math.abs(a.y - b.y) < 1).toBe(true)
    await right.getByRole('button', { name: 'INFO', exact: true }).click()
    await expect(right.getByText(/flood line/)).toHaveCount(0)
    await expect(left.getByText(/flood line 500\b/)).toBeVisible()

    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), scheme).toEqual([])
    }

    // Stacked on a phone.
    await page.setViewportSize({ width: 360, height: 740 })
    const [c, d] = [await left.boundingBox(), await right.boundingBox()]
    expect(c && d && d.y >= c.y + c.height).toBe(true)
    await page.setViewportSize({ width: 1440, height: 900 })

    // An unknown instance gets a note; closing a pane drops it from the URL.
    await page.goto('/logs/shop/main/shop/ghost')
    await expect(page.getByText('No instance “ghost” in project “shop”.')).toBeVisible()
    await page.getByRole('button', { name: 'Close log of shop/ghost' }).click()
    await expect(page).toHaveURL(/\/logs\/shop\/main$/)
    await expect(page.getByRole('region', { name: 'shop/main', exact: true })).toHaveCount(1)
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})

/** Clicks a download in the Export box and returns the file's name and text. */
async function exportAs(page: Page, format: 'log' | 'txt' | 'json'): Promise<{ name: string; text: string }> {
  await page.getByRole('button', { name: /^Export/ }).click()
  const [file]: [Download, unknown] = await Promise.all([
    page.waitForEvent('download'),
    page.getByRole('button', { name: new RegExp(String.raw`Download \.${format}`) }).click(),
  ])
  const path = await file.path()
  return { name: file.suggestedFilename(), text: readFileSync(path, 'utf8') }
}

test('the shown entries export as .log, .txt and .json', async ({ page, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const saved = (await (await client.get(SETTINGS)).json()) as { settings: Record<string, unknown> }
  const put = await client.put(SETTINGS, { ...saved.settings, extra_args: ['--flood', '200', '--simulate', 'traceback'] })
  expect(put.status(), await put.text()).toBe(200)

  try {
    await signIn()
    await page.goto('/projects/shop/instances/main')
    const log = page.getByRole('region', { name: 'Log of shop/main' })
    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)
    await expect(log.getByText(/ValueError: Expected singleton/)).toBeVisible({ timeout: 30_000 })

    // .log: Odoo's own lines, the traceback in full.
    const all = await exportAs(page, 'log')
    expect(all.name).toMatch(/^shop-main-\d{8}-\d{6}\.log$/)
    expect(all.text).toMatch(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d{3} \d+ INFO \S+ odoo\.flood: flood line 200$/m)
    expect(all.text).toMatch(/ ERROR \S+ \S+: Exception during request handling\.\nTraceback \(most recent call last\):\n/)
    expect(all.text.endsWith('\n')).toBe(true)
    await expect(page.getByRole('button', { name: /^Exported \d+ entries/ })).toBeVisible()

    // The open Export box passes contrast in both themes.
    await page.getByRole('button', { name: /^Export/ }).click()
    await expect(page.getByRole('group', { name: 'Export options' })).toBeVisible()
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), `export, ${scheme}`).toEqual([])
    }
    await page.keyboard.press('Escape')
    await expect(page.getByRole('group', { name: 'Export options' })).toBeHidden()

    // .json with only the search matches: flood line 10 and 100–109.
    await page.getByRole('searchbox', { name: 'Search the log' }).fill('flood line 10')
    await expect(page.getByTestId('log-search-status')).toHaveText('11 matches')
    await page.getByRole('button', { name: /^Export/ }).click()
    await page.getByRole('checkbox', { name: 'Only search matches' }).check()
    await page.keyboard.press('Escape')
    const matching = await exportAs(page, 'json')
    const parsed = JSON.parse(matching.text) as {
      project: string
      filter: { search: string; only_matching: boolean }
      entries: { message: string }[]
    }
    expect(parsed.project).toBe('shop')
    expect(parsed.filter).toMatchObject({ search: 'flood line 10', only_matching: true })
    expect(parsed.entries.map((entry) => entry.message)).toEqual([
      'flood line 10',
      ...Array.from({ length: 10 }, (_, i) => `flood line ${100 + i}`),
    ])

    // .txt without INFO: only the traceback is left, the filter in the header.
    await page.getByRole('searchbox', { name: 'Search the log' }).fill('')
    await page.getByRole('button', { name: 'INFO', exact: true }).click()
    const errors = await exportAs(page, 'txt')
    expect(errors.text).toContain('OdHive log export: shop/main\n')
    expect(errors.text).toContain('Levels: DEBUG WARN ERROR CRIT\n')
    expect(errors.text).toContain('ValueError: Expected singleton')
    expect(errors.text).not.toContain('flood line')
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})

test('the SQL log is turned on in the settings and shown as its own source', async ({ page, signIn, request, e2e }) => {
  const client = api(request, e2e)
  const saved = (await (await client.get(SETTINGS)).json()) as { settings: Record<string, unknown> }

  try {
    await signIn()
    await page.goto('/')
    await page.getByRole('button', { name: 'Settings of shop / main' }).click()
    const drawer = page.getByRole('dialog', { name: 'Instance settings' })
    const toggle = drawer.getByRole('checkbox', { name: /^Log SQL queries/ })
    await expect(toggle).not.toBeChecked()
    await toggle.check()
    await drawer.getByRole('button', { name: 'Save' }).click()
    await expect(drawer.getByText(/^Saved/)).toBeVisible()
    const after = (await (await client.get(SETTINGS)).json()) as { settings: { sql_log: boolean } }
    expect(after.settings.sql_log).toBe(true)
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), `settings, ${scheme}`).toEqual([])
    }
    await drawer.getByRole('button', { name: 'Close' }).click()

    await page.goto('/projects/shop/instances/main')
    const log = page.getByRole('region', { name: 'Log of shop/main' })
    const start = await client.post('/api/v1/projects/shop/instances/main/start')
    expect(start.status(), await start.text()).toBe(200)
    const query = log.getByText('query: SELECT "res_users"."id"', { exact: false })
    await expect(query).toBeVisible({ timeout: 30_000 })

    // Hiding DEBUG keeps the queries; the SQL chip hides them.
    await page.getByRole('button', { name: 'DEBUG' }).click()
    await expect(query).toBeVisible()
    const sql = page.getByRole('group', { name: 'Sources' }).getByRole('button', { name: 'SQL' })
    await sql.click()
    await expect(sql).toHaveAttribute('aria-pressed', 'false')
    await expect(query).toHaveCount(0)
    await expect(log.getByText('Registry loaded', { exact: false })).toBeVisible()
    for (const scheme of ['light', 'dark'] as const) {
      await page.emulateMedia({ colorScheme: scheme })
      expect(await seriousViolations(page), `sources, ${scheme}`).toEqual([])
    }
  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})
