import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { routedFetch } from '../test-fixtures'
import type { Job, ManagerEvent } from './api'
import { ApiClient } from './api/client'
import { MAX_BODY, Notifier, hintText, jobToast } from './notices'
import { ToastStore } from './toasts.svelte'

type Body = ManagerEvent extends infer E ? (E extends ManagerEvent ? Omit<E, 'project' | 'instance'> : never) : never

function setup(routes: Parameters<typeof routedFetch>[0] = {}) {
  const { fetcher, calls } = routedFetch(routes)
  const toasts = new ToastStore()
  const notifier = new Notifier(toasts, new ApiClient(fetcher))
  const send = (...events: Body[]): void =>
    notifier.handle({
      type: 'events',
      events: events.map((e) => ({ project: 'shop', instance: 'main', ...e }) as ManagerEvent),
    })
  return { toasts, send, calls }
}

const exit = { code: 1, port_in_use: false, tail: 'Traceback…\nKeyError: x' }

describe('Notifier', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('reports a crash and clears it when the instance runs again', () => {
    const { toasts, send } = setup()
    send({ event: 'status', status: { state: 'crashed', exit } })
    expect(toasts.items).toMatchObject([
      { kind: 'error', title: 'shop/main crashed', body: 'Exit code 1. KeyError: x', timeout: null },
    ])
    send({ event: 'status', status: { state: 'starting', pid: 2 } })
    expect(toasts.items).toHaveLength(1)
    send({ event: 'status', status: { state: 'running', pid: 2 } })
    expect(toasts.items).toHaveLength(0)
  })

  it('reports a failed start and a kill by signal', () => {
    const { toasts, send } = setup()
    send({ event: 'status', status: { state: 'failed_to_start', error: 'odoo-bin not found' } })
    expect(toasts.items[0]).toMatchObject({ title: 'shop/main failed to start', body: 'odoo-bin not found' })
    send({ event: 'status', status: { state: 'crashed', exit: { signal: 9, port_in_use: false, tail: '' } } })
    expect(toasts.items).toMatchObject([{ body: 'Killed by signal 9.' }])
  })

  it('stays quiet for routine events', () => {
    const { toasts, send } = setup()
    send(
      { event: 'spawned', pid: 1, http_port: 8069, gevent_port: 8072, changes: [] },
      { event: 'status', status: { state: 'running', pid: 1 } },
      { event: 'output', line: 'x', stream: 'stderr' },
      { event: 'reload', action: { kind: 'restart' }, file_count: 1, files: ['a.py'] },
      { event: 'reload_watching', roots: ['/a'], modules: 3 },
      { event: 'attached', pid: 1, http_port: 8069, gevent_port: 8072 },
      { event: 'dependency_missing', module: 'requests', package: 'requests', trust: { source: 'table' }, action: { kind: 'install' } },
    )
    expect(toasts.items).toHaveLength(0)
  })

  it('tells about a switched port', () => {
    const { toasts, send } = setup()
    send({
      event: 'spawned',
      pid: 1,
      http_port: 8070,
      gevent_port: 8072,
      changes: [{ kind: 'http', requested: 8069, assigned: 8070, reason: { reason: 'in_use' } }],
    })
    expect(toasts.items).toMatchObject([{ kind: 'info', title: 'shop/main: port 8069 busy', body: 'Using HTTP port 8070 instead.' }])
  })

  it('follows an automatic fix in one toast', () => {
    const { toasts, send } = setup()
    send({ event: 'dependency_installing', module: 'yaml', package: 'PyYAML', requirements: ['PyYAML'], attempt: 1, max: 3 })
    expect(toasts.items).toMatchObject([{ kind: 'info', title: 'shop/main: installing PyYAML' }])
    send({ event: 'dependency_installed', package: 'PyYAML' })
    expect(toasts.items).toMatchObject([{ kind: 'success', title: 'shop/main: installed PyYAML' }])
    send({
      event: 'dependency_failed',
      package: 'PyYAML',
      error: 'uv failed',
      hint: { library: 'compiler', command: 'sudo apt install build-essential', commands: [], manager: 'apt' },
    })
    expect(toasts.items).toMatchObject([{ kind: 'error', body: 'sudo apt install build-essential' }])
  })

  it('asks before installing an untrusted package and confirms over the API', async () => {
    const { toasts, send, calls } = setup({
      'POST /projects/shop/instances/main/fix/confirm': [200, { module: 'zz', package: 'zz', requirements: ['zz'] }],
    })
    send({ event: 'dependency_missing', module: 'zz', package: 'zz', action: { kind: 'confirm' } })
    const [ask] = toasts.items
    expect(ask).toMatchObject({ kind: 'warning', title: 'shop/main: install zz (module “zz”)?', timeout: null })
    expect(ask?.actions?.map((a) => a.label)).toEqual(['Install', 'Not now'])
    await ask?.actions?.[0]?.run()
    expect(calls).toMatchObject([{ method: 'POST', path: '/projects/shop/instances/main/fix/confirm', headers: { 'X-OdHive': '1' } }])
    expect(toasts.items).toHaveLength(0)
  })

  it('declines, and shows a failed answer', async () => {
    const { toasts, send, calls } = setup({
      'POST /projects/shop/instances/main/fix/decline': [409, { error: 'no_pending_fix', message: 'nothing awaits confirmation' }],
    })
    send({ event: 'dependency_missing', module: 'zz', package: 'zz', action: { kind: 'confirm' } })
    await toasts.items[0]?.actions?.[1]?.run()
    expect(calls.map((c) => c.path)).toEqual(['/projects/shop/instances/main/fix/decline'])
    expect(toasts.items).toMatchObject([{ kind: 'error', body: 'nothing awaits confirmation' }])
  })

  it('explains why the fix gives up, and reports with auto-fix off', () => {
    const { toasts, send } = setup()
    send({ event: 'dependency_missing', module: 'x', package: 'x', action: { kind: 'report' } })
    expect(toasts.items.at(-1)?.body).toContain('Auto-fix is off')
    const reasons = [
      [{ reason: 'limit', max: 3 }, 'Already installed 3'],
      [{ reason: 'still_missing', package: 'PyYAML' }, 'still missing'],
      [{ reason: 'refused', refusal: 'part_of_odoo' }, 'part of Odoo'],
      [{ reason: 'refused', refusal: 'invalid_name' }, 'not a valid package name'],
    ] as const
    for (const [reason, text] of reasons) {
      send({ event: 'dependency_missing', module: 'x', action: { kind: 'give_up', reason } })
      expect(toasts.items.at(-1)).toMatchObject({ kind: 'error', title: 'shop/main: missing Python module “x”' })
      expect(toasts.items.at(-1)?.body).toContain(text)
    }
  })

  it('reports system libraries, Smart Reload, new modules, log files and port retries', () => {
    const { toasts, send } = setup()
    send(
      {
        event: 'system_library_missing',
        hint: { library: 'wkhtmltopdf', commands: [{ manager: 'apt', command: 'apt install wkhtmltopdf' }], note: 'Or download it.' },
      },
      { event: 'reload_failed', error: 'inotify limit' },
      { event: 'module_added', module: 'my_mod' },
      { event: 'log_file', error: 'disk full' },
      { event: 'port_retry', http_port: 8069, gevent_port: 8072, exit },
    )
    expect(toasts.items.map((t) => [t.kind, t.title])).toEqual([
      ['warning', 'shop/main: a system library is missing (wkhtmltopdf)'],
      ['warning', 'shop/main: Smart Reload stopped'],
      ['info', 'shop/main: new module “my_mod”'],
      ['warning', 'shop/main: log file not written'],
      ['warning', 'shop/main: ports taken during start'],
    ])
    expect(toasts.items[0]?.body).toBe('apt: apt install wkhtmltopdf\nOr download it.')
    expect(toasts.items[0]?.timeout).toBeNull()
  })

  it('clips long texts', () => {
    const { toasts, send } = setup()
    send({ event: 'reload_failed', error: 'x'.repeat(MAX_BODY * 2) })
    expect(toasts.items[0]?.body).toHaveLength(MAX_BODY)
    expect(hintText({ library: 'ffi', commands: [] })).toBe('')
  })

  it('reports finished jobs', () => {
    const base = { id: 4, started_at: 0, steps: [], output: [], output_dropped: 0 }
    const clone = { ...base, kind: 'clone', version: '19.0' } as const
    expect(jobToast({ ...clone, state: 'running' } as Job)).toBeNull()
    expect(jobToast({ ...clone, state: 'succeeded' } as Job)).toMatchObject({ kind: 'success', title: 'Clone of Odoo 19.0 finished' })
    expect(
      jobToast({ ...base, kind: 'setup', project: 'shop', state: 'failed', error: { error: 'x', message: 'no Python' } } as Job),
    ).toMatchObject({ kind: 'error', title: 'Setup of shop failed', body: 'no Python' })
    expect(jobToast({ ...clone, state: 'cancelled' } as Job)?.kind).toBe('info')

    const { toasts } = setup()
    const notifier = new Notifier(toasts, new ApiClient(routedFetch({}).fetcher))
    notifier.handle({ type: 'job_events', events: [{ event: 'started', job: { ...clone, state: 'running' } as Job }] })
    notifier.handle({ type: 'job_events', events: [{ event: 'finished', job: { ...clone, state: 'succeeded' } as Job }] })
    expect(toasts.items.map((t) => t.title)).toEqual(['Clone of Odoo 19.0 finished'])
  })
})
