import { describe, expect, it } from 'vitest'
import { instanceView, live, project } from '../test-fixtures'
import { allowedActions, instanceState, upCount } from './status'

describe('instanceState', () => {
  it('uses the live status while the service runs the instance', () => {
    const view = instanceView('main', { live: live('shop', 'main', { state: 'running', pid: 1 }) })
    expect(instanceState(view)).toBe('running')
    const crashed = instanceView('main', {
      live: live('shop', 'main', { state: 'crashed', exit: { code: 1, port_in_use: false, tail: '' } }),
      external: { pid: 9, http_port: 8069 },
    })
    expect(instanceState(crashed)).toBe('crashed')
  })

  it('reports instances run elsewhere and stopped ones', () => {
    expect(instanceState(instanceView('a', { external: { pid: 9, http_port: 8069 } }))).toBe('external')
    expect(instanceState(instanceView('a'))).toBe('stopped')
  })
})

describe('upCount', () => {
  it('counts instances that have a process', () => {
    const shop = project('shop', [
      instanceView('a', { live: live('shop', 'a', { state: 'starting', pid: 1 }) }),
      instanceView('b', { external: { pid: 2, http_port: 8070 } }),
      instanceView('c'),
      instanceView('d', { live: live('shop', 'd', { state: 'stopped', exit: { code: 0, port_in_use: false, tail: '' } }) }),
    ])
    expect(upCount(shop)).toBe(2)
  })
})

describe('allowedActions', () => {
  const stopped = { code: 0, port_in_use: false, tail: '' }

  it('follows the instance state', () => {
    const shop = project('shop')
    const at = (status: Parameters<typeof live>[2]) =>
      allowedActions(shop, instanceView('a', { live: live('shop', 'a', status) }))
    expect(at({ state: 'running', pid: 1 })).toEqual({ start: false, stop: true, restart: true })
    expect(at({ state: 'starting', pid: 1 })).toEqual({ start: false, stop: true, restart: true })
    expect(at({ state: 'stopping', pid: 1 })).toEqual({ start: false, stop: false, restart: false })
    expect(at({ state: 'stopped', exit: stopped })).toEqual({ start: true, stop: false, restart: false })
    expect(allowedActions(shop, instanceView('a', { external: { pid: 9, http_port: 8069 } }))).toEqual({
      start: false,
      stop: true,
      restart: false,
    })
  })

  it('never starts without a venv or with a broken project', () => {
    expect(allowedActions(project('a', [], { has_venv: false }), instanceView('x')).start).toBe(false)
    expect(allowedActions(project('a', [], { error: 'bad' }), instanceView('x')).start).toBe(false)
  })
})
