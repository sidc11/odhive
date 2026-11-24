import { card, expect, test } from './fixtures'

test('Ctrl+K palette starts and stops an instance', async ({ page, signIn }) => {
  await signIn()
  const shop = card(page, 'shop')
  await expect(shop).toHaveAttribute('data-state', 'stopped')

  await runAndWait('Start shop/main', 'running')
  await runAndWait('Stop shop/main', 'stopped')

  // The top bar button opens it too; its name includes the visible shortcut.
  const button = page.getByRole('button', { name: /^Command palette\s*Ctrl\s*K$/ })
  await button.click()
  await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(button).toBeFocused()

  async function runAndWait(title: string, state: string) {
    await page.keyboard.press('Control+K')
    const search = page.getByRole('combobox', { name: 'Search commands' })
    await expect(search).toBeFocused()
    await search.fill(title)
    await expect(page.getByRole('option').first()).toHaveAccessibleName(new RegExp(`^${title}\\b`))
    await search.press('Enter')
    await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeHidden()
    await expect(shop).toHaveAttribute('data-state', state, { timeout: 30_000 })
  }
})
