import type { Page } from '@playwright/test'
import { expect, seriousViolations, test } from './fixtures'

// The e2e service has no PostgreSQL server, so the database routes are
// answered here; the request flows themselves are covered by Vitest and
// the server's own tests.
const settings = { socket_dirs: ['/run/postgresql'], port: 5432, user: 'dev', password_set: false, ssl_mode: 'prefer' }
const databases = {
  databases: [
    {
      name: 'shop_main',
      valid_name: true,
      odoo: true,
      odoo_series: '19.0',
      odoo_version: '19.0.1.3',
      size_bytes: 25_165_824,
      last_modified: '2026-11-26T09:05:00Z',
      instances: ['main'],
    },
    { name: 'scratch', valid_name: true, odoo: false, size_bytes: 8_000_000, instances: [] },
  ],
  hidden: 0,
  settings,
}
const snapshots = [
  {
    name: 'shop_main__snap_20261126090500',
    database: 'shop_main',
    created_at: Date.UTC(2026, 10, 26, 9, 5),
    filestore: true,
    broken: false,
    size: 25_165_824,
  },
]
const backups = [
  { file: 'shop_main_20261126-090500.zip', database: 'shop_main', format: 'zip', size: 4_200_000, modified_at: Date.UTC(2026, 10, 26, 9, 5) },
]

async function mockDatabases(page: Page): Promise<void> {
  const json = (body: unknown) => ({ status: 200, contentType: 'application/json', body: JSON.stringify(body) })
  await page.route('**/api/v1/projects/shop/databases', (route) =>
    route.request().method() === 'GET' ? route.fulfill(json(databases)) : route.fallback(),
  )
  await page.route('**/api/v1/projects/shop/snapshots', (route) => route.fulfill(json(snapshots)))
  await page.route('**/api/v1/projects/shop/backups', (route) => route.fulfill(json(backups)))
}

test('the database page lists databases and opens dialogs that keep focus', async ({ page, signIn }) => {
  await mockDatabases(page)
  await signIn()
  await page.goto('/projects/shop')
  await page.getByRole('link', { name: 'Databases' }).click()
  await expect(page).toHaveURL(/\/projects\/shop\/databases$/)
  await expect(page.getByRole('heading', { level: 1, name: 'Databases' })).toBeVisible()

  const main = page.getByRole('row', { name: /^shop_main/ })
  await expect(main.getByRole('cell', { name: '19.0.1.3' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Download' })).toHaveAttribute(
    'href',
    '/api/v1/projects/shop/backups/shop_main_20261126-090500.zip',
  )

  const del = main.getByRole('button', { name: 'Delete' })
  await del.click()
  const dialog = page.getByRole('dialog', { name: 'Delete database' })
  await expect(dialog).toBeVisible()
  // Tab stays inside the dialog.
  for (let i = 0; i < 6; i++) await page.keyboard.press('Tab')
  await expect(dialog.locator(':focus')).toHaveCount(1)
  await expect(dialog.getByRole('button', { name: 'Delete', exact: true })).toBeDisabled()
  await dialog.getByLabel(/to confirm/).fill('shop_main')
  await expect(dialog.getByRole('button', { name: 'Delete', exact: true })).toBeEnabled()
  await page.keyboard.press('Escape')
  await expect(dialog).toBeHidden()
  await expect(del).toBeFocused()
})

for (const scheme of ['light', 'dark'] as const) {
  test(`database page and its dialog have no serious accessibility violations (${scheme})`, async ({ page, signIn }) => {
    await page.emulateMedia({ colorScheme: scheme })
    await mockDatabases(page)
    await signIn()
    await page.goto('/projects/shop/databases')
    await expect(page.getByRole('row', { name: /^shop_main/ })).toBeVisible()
    expect(await seriousViolations(page)).toEqual([])

    await page.getByRole('button', { name: 'New database…' }).click()
    await expect(page.getByRole('dialog', { name: 'New database' })).toBeVisible()
    await page.getByRole('dialog').getByRole('button', { name: 'Create' }).click()
    await expect(page.getByText('Enter a password.')).toBeVisible()
    expect(await seriousViolations(page)).toEqual([])
  })
}

test('the database page fits a phone screen', async ({ page, signIn }) => {
  await page.setViewportSize({ width: 360, height: 800 })
  await mockDatabases(page)
  await signIn()
  await page.goto('/projects/shop/databases')
  await expect(page.getByRole('row', { name: /^shop_main/ })).toBeVisible()
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
  expect(overflow).toBeLessThanOrEqual(0)
  await page.getByRole('button', { name: 'Restore a backup…' }).click()
  const dialog = page.getByRole('dialog', { name: 'Restore a backup' })
  await expect(dialog).toBeInViewport()
  await expect(dialog.getByRole('button', { name: 'Upload and restore' })).toBeInViewport()
})
