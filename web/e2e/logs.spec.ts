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

  } finally {
    await client.post('/api/v1/projects/shop/instances/main/stop')
    await client.put(SETTINGS, saved.settings)
  }
})
