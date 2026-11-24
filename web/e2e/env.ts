// End-to-end environment: a private HOME with a fake Odoo 19 source and a
// ready project, and the real `odhive` binary serving the UI from it.
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
export const repoRoot = resolve(here, '../..')

/** Where global setup leaves what the tests need. */
export const stateFile = join(here, '.state.json')

export interface E2eState {
  home: string
  /** Where the fake editor writes the arguments it was started with. */
  editorLog: string
  baseURL: string
  /** Bearer token of the service (for one-time sign-in codes). */
  token: string
  pid: number
}

export function readState(): E2eState {
  return JSON.parse(readFileSync(stateFile, 'utf8')) as E2eState
}

export function binary(): string {
  return process.env.ODHIVE_BIN ?? join(repoRoot, 'target/debug/odhive')
}

/** Environment for `odhive`: everything lives in `home`, never a browser. */
export function odhiveEnv(home: string): NodeJS.ProcessEnv {
  const dropped = new Set(['BROWSER', 'DISPLAY', 'WAYLAND_DISPLAY', 'RUST_LOG'])
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !dropped.has(key)))
  return {
    ...env,
    HOME: home,
    XDG_CONFIG_HOME: join(home, '.config'),
    XDG_DATA_HOME: join(home, '.local/share'),
  }
}

export function odhive(home: string, args: string[]): string {
  return execFileSync(binary(), args, { env: odhiveEnv(home), encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] })
}

/** A fresh HOME with a registered fake Odoo 19 source (`odoo-19.0`). */
export function makeHome(): string {
  const home = realpathSync(mkdtempSync(join(tmpdir(), 'odhive-e2e-')))
  const src = join(home, 'odoo-src')
  mkdirSync(join(src, 'odoo'), { recursive: true })
  mkdirSync(join(src, 'addons'), { recursive: true })
  copyFileSync(join(repoRoot, 'tests/fixtures/fake-odoo-bin'), join(src, 'odoo-bin'))
  chmodSync(join(src, 'odoo-bin'), 0o755)
  writeFileSync(join(src, 'odoo/release.py'), "version_info = (19, 0, 0, FINAL, 0, '')\n")
  writeFileSync(join(src, 'requirements.txt'), '')
  odhive(home, ['source', 'add', src])
  return home
}

export function removeHome(home: string): void {
  rmSync(home, { recursive: true, force: true })
}

export async function freePort(): Promise<number> {
  return new Promise((done, fail) => {
    const server = createServer()
    server.once('error', fail)
    server.listen(0, '127.0.0.1', () => {
      const address = server.address()
      const port = typeof address === 'object' && address ? address.port : 0
      server.close(() => done(port))
    })
  })
}

/** Starts `odhive serve --foreground` and waits for its sign-in line. */
export async function serve(home: string, port: number): Promise<{ child: ChildProcess; baseURL: string }> {
  const child = spawn(binary(), ['--port', String(port), 'serve', '--foreground'], {
    env: odhiveEnv(home),
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let output = ''
  const baseURL = await new Promise<string>((done, fail) => {
    const timer = setTimeout(() => fail(new Error(`odhive did not start in time:\n${output}`)), 30_000)
    const onData = (chunk: Buffer) => {
      output += chunk.toString()
      const match = /Open OdHive.*?(http:\/\/localhost:\d+)\//.exec(output)
      if (match?.[1]) {
        clearTimeout(timer)
        done(match[1])
      }
    }
    child.stdout?.on('data', onData)
    child.stderr?.on('data', onData)
    child.once('exit', (code) => {
      clearTimeout(timer)
      fail(new Error(`odhive exited (${code}) before it was ready:\n${output}`))
    })
  })
  // Keep draining so a full pipe never blocks the service.
  child.stdout?.resume()
  child.stderr?.resume()
  return { child, baseURL }
}

/** Config folder of `odhive` (the same as the data folder on macOS). */
export function configDir(home: string): string {
  return process.platform === 'darwin' ? dataDir(home) : join(home, '.config/odhive')
}

/**
 * Configures a fake editor that writes its arguments (one per line) to the
 * returned file, so open-in-editor can be checked without a real editor.
 */
export function fakeEditor(home: string): string {
  const log = join(home, 'editor-args.txt')
  const script = join(home, 'fake-editor')
  writeFileSync(script, `#!/bin/sh\nprintf '%s\\n' "$@" > '${log}'\n`)
  chmodSync(script, 0o755)
  // A top-level key goes before the first table.
  const config = join(configDir(home), 'config.toml')
  writeFileSync(config, `editor = [${JSON.stringify(script)}, "{file}:{line}"]\n${readFileSync(config, 'utf8')}`)
  return log
}

export function dataDir(home: string): string {
  return process.platform === 'darwin'
    ? join(home, 'Library/Application Support/odhive')
    : join(home, '.local/share/odhive')
}
