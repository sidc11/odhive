import { expect, seriousViolations, test } from './fixtures'

const pages = [
  { name: 'dashboard', path: '/', heading: 'Dashboard' },
  { name: 'wizard', path: '/new', heading: 'New project' },
  { name: 'instance page', path: '/projects/shop/instances/main', heading: 'main' },
]

for (const scheme of ['light', 'dark'] as const) {
  for (const target of pages) {
    test(`${target.name} has no serious accessibility violations (${scheme})`, async ({ page, signIn }) => {
      await page.emulateMedia({ colorScheme: scheme })
      await signIn()
      await page.goto(target.path)
      await expect(page.getByRole('heading', { level: 1, name: target.heading })).toBeVisible()
      await expect(page.locator('html')).toHaveAttribute('data-theme', scheme)

      expect(await seriousViolations(page)).toEqual([])
    })
  }
}
