import { expect, test } from './fixtures'

const widths = [360, 768, 1440]
const pages = [
  { path: '/', heading: 'Dashboard' },
  { path: '/new', heading: 'New project' },
  { path: '/environment', heading: 'Environment' },
  { path: '/projects/shop/instances/main', heading: 'main' },
]

for (const width of widths) {
  test(`pages fit ${width}px without sideways scrolling`, async ({ page, signIn }) => {
    await page.setViewportSize({ width, height: 800 })
    await signIn()
    for (const target of pages) {
      await page.goto(target.path)
      await expect(page.getByRole('heading', { level: 1, name: target.heading })).toBeVisible()
      const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
      expect(overflow, `${target.path} at ${width}px`).toBeLessThanOrEqual(0)
    }

    // Below 768px the sidebar is a drawer behind the menu button.
    const menu = page.getByRole('button', { name: 'Open navigation' })
    if (width < 768) {
      await menu.click()
      await expect(environmentLink()).toBeVisible()
      await page.keyboard.press('Escape')
      await expect(menu).toBeFocused()
    } else {
      await expect(menu).toBeHidden()
      await expect(environmentLink()).toBeVisible()
    }

    function environmentLink() {
      return page.getByRole('link', { name: 'Environment', exact: true }).filter({ visible: true })
    }
  })
}
