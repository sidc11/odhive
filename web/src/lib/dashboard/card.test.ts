import { describe, expect, it } from 'vitest'
import { live } from '../../test-fixtures'
import type { PortChange } from '../api'
import { exitSummary, gitSummary, odooUrl, portMove } from './card'

const exit = { port_in_use: false, tail: '' }

describe('card helpers', () => {
  it('builds loopback Odoo URLs', () => {
    expect(odooUrl(8070)).toBe('http://127.0.0.1:8070/web')
    expect(odooUrl(8070, true)).toBe('http://127.0.0.1:8070/web?debug=assets')
  })

  it('reports an HTTP port move with its reason', () => {
    const moved: PortChange = {
      kind: 'http',
      requested: 8069,
      assigned: 8070,
      reason: { reason: 'reserved_by', owner: { project: 'crm', instance: 'main' } },
    }
    const gevent: PortChange = { kind: 'gevent', requested: 8072, assigned: 8073, reason: { reason: 'in_use' } }
    const info = { ...live('shop', 'main', { state: 'running', pid: 1 }), http_port: 8070 }
    expect(portMove({ ...info, port_changes: [gevent, moved] })).toEqual({
      from: 8069,
      reason: '8069 is used by crm/main; using 8070',
    })
    expect(portMove({ ...info, port_changes: [gevent] })).toBeNull()
    // Ports were given up (stopped): nothing to explain.
    expect(portMove({ ...info, http_port: null, port_changes: [moved] })).toBeNull()
    expect(portMove(null)).toBeNull()
  })

  it('summarises git heads', () => {
    expect(gitSummary([])).toBeNull()
    expect(
      gitSummary([
        { repo: '/p/shop', branch: 'main', commit: null },
        { repo: '/src/oca', branch: null, commit: 'abc1234' },
      ]),
    ).toEqual({ label: 'main +1', title: '/p/shop: main\n/src/oca: detached at abc1234' })
  })

  it('explains bad exits only', () => {
    expect(exitSummary(undefined)).toBeNull()
    expect(exitSummary({ state: 'running', pid: 1 })).toBeNull()
    expect(exitSummary({ state: 'stopped', exit: { ...exit, code: 0 } })).toBeNull()
    expect(exitSummary({ state: 'crashed', exit: { ...exit, code: 1 } })).toBe('Crashed with exit code 1')
    expect(exitSummary({ state: 'crashed', exit: { ...exit, signal: 9 } })).toBe('Crashed: killed by signal 9')
    expect(exitSummary({ state: 'crashed', exit: { ...exit, port_in_use: true, code: 1 } })).toBe(
      'Crashed: a port was already in use',
    )
    expect(exitSummary({ state: 'failed_to_start', error: 'no venv' })).toBe('Failed to start: no venv')
  })
})
