import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import { instanceView, live, project } from '../../test-fixtures'
import type { InstanceUsage, InstanceView, Project } from '../api'
import InstanceCard from './InstanceCard.svelte'

const NOW = 1_700_000_000_000
const USAGE: InstanceUsage = {
  project: 'shop',
  instance: 'main',
  pid: 7,
  cpu_percent: 12.4,
  memory_bytes: 300 * 1024 * 1024,
  processes: 3,
  started_at: NOW / 1000 - 192,
}

function show(view: InstanceView, extra: { project?: Project; usage?: InstanceUsage; busy?: 'start' | 'stop' | 'restart' } = {}) {
  const onaction = vi.fn()
  const p = extra.project ?? project('shop', [view], { git: [{ repo: '/p/shop', branch: 'feature/pos', commit: null }] })
  const onsettings = vi.fn()
  render(InstanceCard, { project: p, view, usage: extra.usage, now: NOW, busy: extra.busy, onaction, onsettings })
  return { onaction, onsettings, card: screen.getByRole('article', { name: `shop / ${view.id}` }) }
}

function field(card: HTMLElement, name: string): string {
  const term = within(card).getByText(name, { selector: 'dt' })
  return term.nextElementSibling?.textContent?.trim() ?? ''
}

describe('InstanceCard', () => {
  it('opens the settings of its instance', async () => {
    const { card, onsettings, onaction } = show(instanceView('main'))
    await fireEvent.click(within(card).getByRole('button', { name: 'Settings of shop / main' }))
    expect(onsettings).toHaveBeenCalledOnce()
    expect(onaction).not.toHaveBeenCalled()
  })

  it('shows a running instance with its details, usage and actions', async () => {
    const running = {
      ...live('shop', 'main', { state: 'running', pid: 7 }),
      http_port: 8070,
      port_changes: [{ kind: 'http', requested: 8069, assigned: 8070, reason: { reason: 'in_use' } } as const],
    }
    const { card, onaction } = show(instanceView('main', { db: 'shop_main', live: running }), { usage: USAGE })
    expect(card).toHaveAttribute('data-state', 'running')
    expect(within(card).getByText('Odoo 19.0')).toBeInTheDocument()
    expect(field(card, 'Port')).toContain('8070')
    expect(within(card).getByTestId('port-moved')).toHaveTextContent('switched from 8069')
    expect(within(card).getByTestId('port-moved')).toHaveAttribute('title', '8069 was busy; using 8070')
    expect(field(card, 'Database')).toBe('db_main')
    expect(field(card, 'Branch')).toBe('feature/pos')
    expect(field(card, 'Uptime')).toBe('3m 12s')
    expect(field(card, 'CPU')).toBe('12%')
    expect(field(card, 'Memory')).toBe('300 MiB')

    expect(within(card).queryByRole('button', { name: 'Start' })).toBeNull()
    await fireEvent.click(within(card).getByRole('button', { name: 'Stop' }))
    await fireEvent.click(within(card).getByRole('button', { name: 'Restart' }))
    expect(onaction.mock.calls).toEqual([['stop'], ['restart']])

    const open = within(card).getByRole('link', { name: /^Open/ })
    expect(open).toHaveAttribute('href', 'http://127.0.0.1:8070/web')
    expect(open).toHaveAttribute('target', '_blank')
    expect(open).toHaveAttribute('rel', 'noopener noreferrer')
    expect(within(card).getByRole('link', { name: /^Debug/ })).toHaveAttribute(
      'href',
      'http://127.0.0.1:8070/web?debug=assets',
    )
  })

  it('offers Start for a stopped instance and hides usage and links', async () => {
    const { card, onaction } = show(instanceView('main', { http_port: 8069 }), { usage: USAGE })
    expect(field(card, 'Port')).toBe('8069')
    expect(field(card, 'Uptime')).toBe('—')
    expect(field(card, 'Memory')).toBe('—')
    expect(within(card).queryByRole('link', { name: /^Open/ })).toBeNull()
    expect(within(card).queryByRole('button', { name: 'Restart' })).toBeNull()
    await fireEvent.click(within(card).getByRole('button', { name: 'Start' }))
    expect(onaction).toHaveBeenCalledWith('start')
  })

  it('cannot start without a venv, and says why', () => {
    const view = instanceView('main')
    const { card } = show(view, { project: project('shop', [view], { has_venv: false }) })
    expect(within(card).getByRole('button', { name: 'Start' })).toBeDisabled()
    expect(within(card).getByText(/Python environment/)).toBeInTheDocument()
  })

  it('shows why it crashed', () => {
    const crashed = live('shop', 'main', { state: 'crashed', exit: { code: 3, port_in_use: false, tail: '' } })
    const { card } = show(instanceView('main', { live: crashed }))
    expect(within(card).getByText('Crashed with exit code 3')).toBeInTheDocument()
    expect(within(card).getByRole('button', { name: 'Start' })).toBeEnabled()
  })

  it('can only stop an instance run outside this service', () => {
    const view = instanceView('main', { external: { pid: 9, http_port: 8075, supervisor: null } })
    const { card } = show(view)
    expect(field(card, 'Port')).toBe('8075')
    expect(within(card).getByRole('button', { name: 'Stop' })).toBeEnabled()
    expect(within(card).queryByRole('button', { name: 'Restart' })).toBeNull()
    expect(within(card).getByRole('link', { name: /^Open/ })).toHaveAttribute('href', 'http://127.0.0.1:8075/web')
  })

  it('disables the actions while one is under way', () => {
    const view = instanceView('main', { live: live('shop', 'main', { state: 'running', pid: 1 }) })
    const { card } = show(view, { busy: 'restart' })
    expect(within(card).getByRole('button', { name: 'Stop' })).toBeDisabled()
    expect(within(card).getByRole('button', { name: 'Restarting…' })).toBeDisabled()
  })
})
