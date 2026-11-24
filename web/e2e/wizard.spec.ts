import { card, expect, test } from './fixtures'

test('sign in, create a project with the wizard, start it and stop it', async ({ page, signIn }) => {
  test.setTimeout(180_000)
  // Unique, so a repeated run against the same service gets a new project.
  const name = `wiz-${Date.now().toString(36)}`
  await signIn()

  await page.getByRole('link', { name: 'New project' }).first().click()
  await expect(page).toHaveURL(/\/new$/)
  await expect(page.getByRole('heading', { level: 1, name: 'New project' })).toBeVisible()

  await page.getByLabel('Project name').fill(name)
  await page.getByRole('button', { name: 'Next' }).click()

  // The registered fake Odoo 19 source.
  await page.getByRole('radio', { name: /Odoo 19\.0 · odoo-19\.0/ }).check()
  // Enterprise, addons and Python keep their defaults (none, none, automatic).
  const create = page.getByRole('button', { name: 'Create project' })
  for (let step = 0; step < 6 && !(await create.isVisible()); step += 1) {
    await page.getByRole('button', { name: 'Next' }).click()
  }
  await expect(page.getByLabel('Instance id')).toHaveValue('main')
  await create.click()

  // Project created, then its setup (a real venv) followed live.
  await expect(page.getByRole('heading', { name: `${name} is ready` })).toBeVisible({ timeout: 120_000 })
  await page.getByRole('region', { name: `${name} is ready` }).getByRole('link', { name: 'Dashboard' }).click()
  await expect(page).toHaveURL(/\/$/)

  const project = card(page, name)
  await expect(project).toHaveAttribute('data-state', 'stopped')
  await project.getByRole('button', { name: 'Start' }).click()
  await expect(project).toHaveAttribute('data-state', 'running', { timeout: 30_000 })
  // The first detail is the port the instance runs on.
  await expect(project.getByRole('definition').first()).toHaveText(/^\d{4,5}\b/)
  await expect(project.getByRole('link', { name: /^Open/ }).first()).toHaveAttribute(
    'href',
    /^http:\/\/127\.0\.0\.1:\d+\/web$/,
  )

  await project.getByRole('button', { name: 'Stop' }).click()
  await expect(project).toHaveAttribute('data-state', 'stopped', { timeout: 30_000 })
})
