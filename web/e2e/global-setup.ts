import { readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { dataDir, freePort, makeHome, odhive, removeHome, serve, stateFile, type E2eState } from './env'

/**
 * A private HOME with a fake Odoo 19 source and a set-up project `shop`, and
 * `odhive serve --foreground` on a free port. Returns the teardown.
 */
export default async function globalSetup(): Promise<() => Promise<void>> {
  const home = makeHome()
  // A ready project for the palette and layout tests (real venv, no requirements).
  odhive(home, ['project', 'add', 'shop', '--source', 'odoo-19.0', '--no-requirements'])
  const { child, baseURL } = await serve(home, await freePort())
  const token = readFileSync(join(dataDir(home), 'token'), 'utf8').trim()
  const state: E2eState = { home, baseURL, token, pid: child.pid ?? 0 }
  writeFileSync(stateFile, JSON.stringify(state))

  return async () => {
    // SIGINT = Ctrl+C: the service stops its instances, then exits.
    if (child.exitCode === null) {
      const exited = new Promise((done) => child.once('exit', done))
      child.kill('SIGINT')
      await Promise.race([exited, new Promise((done) => setTimeout(done, 30_000))])
      if (child.exitCode === null) child.kill('SIGKILL')
    }
    rmSync(stateFile, { force: true })
    removeHome(home)
  }
}
