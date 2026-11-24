import { defineConfig, devices } from '@playwright/test'

// End-to-end tests drive the real `odhive` binary (with the UI embedded) and
// the fake odoo-bin. Build both first: `pnpm build && cargo build -p odhive`.
// One service is shared by all tests, so they run one at a time.
export default defineConfig({
  testDir: './e2e',
  globalSetup: './e2e/global-setup.ts',
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  use: {
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
})
