import AxeBuilder from '@axe-core/playwright'
import { expect, test as base, type Page } from '@playwright/test'
import { readState, type E2eState } from './env'

interface Fixtures {
  e2e: E2eState
  /** Signs the page in through a fresh one-time URL, like the launcher does. */
  signIn: () => Promise<void>
}

export const test = base.extend<Fixtures>({
  // Playwright needs the destructuring pattern even when no fixture is used.
  // eslint-disable-next-line no-empty-pattern
  e2e: async ({}, use) => {
    await use(readState())
  },
  baseURL: async ({ e2e }, use) => {
    await use(e2e.baseURL)
  },
  signIn: async ({ page, e2e, request }, use) => {
    await use(async () => {
      const resp = await request.post(`${e2e.baseURL}/api/v1/auth/codes`, {
        headers: { Authorization: `Bearer ${e2e.token}`, 'X-OdHive': '1' },
      })
      expect(resp.status(), await resp.text()).toBe(201)
      const { path } = (await resp.json()) as { path: string }
      expect(path).toMatch(/^\/login\?t=[0-9a-f]{64}$/)
      await page.goto(path)
      await expect(page).toHaveURL(`${e2e.baseURL}/`)
      await expect(page.getByRole('heading', { level: 1, name: 'Dashboard' })).toBeVisible()
    })
  },
})

export { expect }

/** The dashboard card of `project/instance`. */
export function card(page: Page, project: string, instance = 'main') {
  return page.getByRole('article', { name: `${project} / ${instance}`, exact: true })
}


/** Serious and critical WCAG 2.1 AA violations axe finds on the page. */
export async function seriousViolations(page: Page) {
  const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa']).analyze()
  return result.violations
    .filter((v) => v.impact === 'serious' || v.impact === 'critical')
    .map((v) => ({ id: v.id, help: v.help, targets: v.nodes.map((n) => n.target.join(' ')) }))
}
