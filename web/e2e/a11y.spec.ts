import AxeBuilder from '@axe-core/playwright'
import { expect, test } from './fixtures'

const pages = [
  { name: 'dashboard', path: '/', heading: 'Dashboard' },
  { name: 'wizard', path: '/new', heading: 'New project' },
]

for (const scheme of ['light', 'dark'] as const) {
  for (const target of pages) {
    test(`${target.name} has no serious accessibility violations (${scheme})`, async ({ page, signIn }) => {
      await page.emulateMedia({ colorScheme: scheme })
      await signIn()
      await page.goto(target.path)
      await expect(page.getByRole('heading', { level: 1, name: target.heading })).toBeVisible()
      await expect(page.locator('html')).toHaveAttribute('data-theme', scheme)

      const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa']).analyze()
      const serious = result.violations
        .filter((v) => v.impact === 'serious' || v.impact === 'critical')
        .map((v) => ({ id: v.id, help: v.help, targets: v.nodes.map((n) => n.target.join(' ')) }))
      expect(serious).toEqual([])
    })
  }
}
