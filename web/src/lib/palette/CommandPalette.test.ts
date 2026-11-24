import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import type { ProjectsState } from '../projects.svelte'
import { instanceView, live, project } from '../../test-fixtures'
import CommandPalette from './CommandPalette.svelte'
import { PaletteStore } from './palette.svelte'

const READY: ProjectsState = {
  status: 'ready',
  projects: [
    project('shop', [
      instanceView('main', { live: live('shop', 'main', { state: 'running', pid: 1 }) }),
      instanceView('test'),
    ]),
    project('crm', [instanceView('main')]),
  ],
}

function setup(state: ProjectsState = READY) {
  const palette = new PaletteStore()
  const deps = { navigate: vi.fn(), act: vi.fn(), settings: vi.fn() }
  render(CommandPalette, { palette, projects: { state }, deps })
  return { palette, deps }
}

async function open(): Promise<HTMLElement> {
  await fireEvent.keyDown(window, { key: 'k', ctrlKey: true })
  const input = await screen.findByRole('combobox', { name: 'Search commands' })
  await vi.waitFor(() => expect(input).toHaveFocus())
  return input
}

const optionNames = (): string[] =>
  screen.getAllByRole('option').map((option) => option.getAttribute('aria-label') ?? '')

const selected = (): HTMLElement => {
  const input = screen.getByRole('combobox')
  const id = input.getAttribute('aria-activedescendant') ?? ''
  const option = document.getElementById(id)
  if (!option) throw new Error('no active option')
  expect(option).toHaveAttribute('aria-selected', 'true')
  return option
}

describe('CommandPalette', () => {
  it('opens with Ctrl+K and Cmd+K, closes with Esc and the shortcut', async () => {
    const { palette } = setup()
    expect(screen.queryByRole('dialog')).toBeNull()
    const input = await open()
    expect(screen.getByRole('dialog', { name: 'Command palette' })).toBeInTheDocument()
    await fireEvent.keyDown(input, { key: 'Escape' })
    expect(palette.open).toBe(false)
    expect(screen.queryByRole('dialog')).toBeNull()

    await fireEvent.keyDown(window, { key: 'k', metaKey: true })
    expect(await screen.findByRole('dialog')).toBeInTheDocument()
    await fireEvent.keyDown(window, { key: 'k', ctrlKey: true })
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('stops the browser’s own Ctrl+K', async () => {
    setup()
    const event = new KeyboardEvent('keydown', { key: 'k', ctrlKey: true, cancelable: true })
    window.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(true)
    const plain = new KeyboardEvent('keydown', { key: 'k', cancelable: true })
    window.dispatchEvent(plain)
    expect(plain.defaultPrevented).toBe(false)
  })

  it('lists commands under group headings when the query is empty', async () => {
    setup()
    await open()
    const groups = screen.getAllByRole('group').map((group) => group.getAttribute('aria-label'))
    expect(groups).toEqual(['Pages', 'Projects', 'Instances', 'Actions'])
    expect(optionNames()).toContain('Stop shop/main, Running')
    expect(optionNames()).toContain('Start shop/test, Stopped')
    expect(optionNames()).toContain('shop/main, Running, Database db_main')
    expect(optionNames().filter((name) => name.startsWith('Start shop/main'))).toEqual([])
    expect(optionNames()).toContain('Start crm/main, Stopped')
    expect(selected()).toHaveTextContent('Dashboard')
  })

  it('filters as you type, highlights the match and announces the count', async () => {
    setup()
    const input = await open()
    await fireEvent.input(input, { target: { value: 'rst sh main' } })
    expect(screen.getAllByRole('group').map((group) => group.getAttribute('aria-label'))).toEqual(['Results'])
    const first = screen.getAllByRole('option')[0]
    expect(first).toHaveTextContent('Restart shop/main')
    expect(selected()).toBe(first)
    expect(within(first as HTMLElement).getAllByText((_, el) => el?.tagName === 'MARK').length).toBeGreaterThan(0)
    expect(screen.getByRole('status')).toHaveTextContent(/^\d+ results?$/)

    await fireEvent.input(input, { target: { value: 'zzzz' } })
    expect(screen.queryAllByRole('option')).toHaveLength(0)
    expect(screen.getByText('No commands match “zzzz”.')).toBeInTheDocument()
    expect(screen.getByRole('status')).toHaveTextContent('No results')
    expect(input).not.toHaveAttribute('aria-activedescendant')
    // Enter with nothing to run does nothing.
    await fireEvent.keyDown(input, { key: 'Enter' })
    expect(screen.getByRole('dialog')).toBeInTheDocument()
  })

  it('moves with the arrows (wrapping) and runs the selection with Enter', async () => {
    const { deps, palette } = setup()
    const input = await open()
    await fireEvent.input(input, { target: { value: 'shop' } })
    const count = screen.getAllByRole('option').length
    expect(count).toBeGreaterThan(2)
    await fireEvent.keyDown(input, { key: 'ArrowUp' })
    expect(selected()).toBe(screen.getAllByRole('option')[count - 1])
    await fireEvent.keyDown(input, { key: 'ArrowDown' })
    expect(selected()).toBe(screen.getAllByRole('option')[0])
    await fireEvent.keyDown(input, { key: 'ArrowDown' })
    const second = selected().textContent

    await fireEvent.keyDown(input, { key: 'Enter' })
    expect(palette.open).toBe(false)
    expect(deps.navigate.mock.calls.length + deps.act.mock.calls.length).toBe(1)
    expect(second).toBeTruthy()
  })

  it('starts an instance', async () => {
    const { deps } = setup()
    const input = await open()
    await fireEvent.input(input, { target: { value: 'start test' } })
    expect(selected()).toHaveTextContent('Start shop/test')
    await fireEvent.keyDown(input, { key: 'Enter' })
    expect(deps.act).toHaveBeenCalledWith('start', 'shop', 'test')
    expect(deps.navigate).not.toHaveBeenCalled()
  })

  it('runs a clicked option and closes on a backdrop click', async () => {
    const { deps } = setup()
    await open()
    await fireEvent.click(screen.getByRole('option', { name: /Environment/ }))
    expect(deps.navigate).toHaveBeenCalledWith('/environment')
    expect(screen.queryByRole('dialog')).toBeNull()

    await open()
    await fireEvent.click(screen.getByRole('button', { name: 'Close command palette' }))
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('starts over with an empty query and keeps Tab in the field', async () => {
    setup()
    let input = await open()
    await fireEvent.input(input, { target: { value: 'crm' } })
    await fireEvent.keyDown(input, { key: 'Escape' })
    input = await open()
    expect(input).toHaveValue('')
    const tab = new KeyboardEvent('keydown', { key: 'Tab', cancelable: true, bubbles: true })
    input.dispatchEvent(tab)
    expect(tab.defaultPrevented).toBe(true)
  })

  it('still offers pages while projects load', async () => {
    setup({ status: 'loading' })
    await open()
    expect(optionNames()).toEqual([
      'Dashboard',
      'Environment, PostgreSQL, Python, wkhtmltopdf, git, uv',
      'Logs, Up to 4 instance logs side by side',
      'New project, Open the project wizard',
    ])
  })
})
