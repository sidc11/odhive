import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { tick } from 'svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DEFAULT_TIMEOUT_MS, ToastStore } from '../toasts.svelte'
import Toaster from './Toaster.svelte'

describe('Toaster', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('shows toasts in a live region; errors as alerts', async () => {
    const toasts = new ToastStore()
    render(Toaster, { toasts })
    toasts.push({ kind: 'info', title: 'Port switched', body: 'Using 8070.' })
    toasts.push({ kind: 'error', title: 'shop/main crashed' })
    await tick()
    const region = screen.getByRole('region', { name: 'Notifications' })
    expect(region.querySelectorAll('li')).toHaveLength(2)
    expect(region.querySelector('[aria-live="polite"]')).not.toBeNull()
    expect(screen.getByRole('alert')).toHaveTextContent('Error: shop/main crashed')
    expect(screen.getByText('Using 8070.')).toBeInTheDocument()
  })

  it('dismisses with the close button', async () => {
    const toasts = new ToastStore()
    render(Toaster, { toasts })
    toasts.push({ kind: 'error', title: 'boom' })
    await fireEvent.click(await screen.findByRole('button', { name: 'Dismiss notification' }))
    expect(toasts.items).toHaveLength(0)
  })

  it('runs an action once, disabling the buttons meanwhile', async () => {
    const toasts = new ToastStore()
    render(Toaster, { toasts })
    let finish: () => void = () => {}
    const run = vi.fn(() => new Promise<void>((resolve) => (finish = resolve)))
    toasts.push({ kind: 'warning', title: 'Install zz?', actions: [{ label: 'Install', run }, { label: 'Not now', run: vi.fn() }] })
    const install = await screen.findByRole('button', { name: 'Install' })
    await fireEvent.click(install)
    await fireEvent.click(install)
    expect(run).toHaveBeenCalledTimes(1)
    expect(install).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Not now' })).toBeDisabled()
    finish()
    await vi.waitFor(() => expect(install).toBeEnabled())
  })

  it('keeps a toast while the pointer is over it', async () => {
    const toasts = new ToastStore()
    render(Toaster, { toasts })
    toasts.push({ kind: 'success', title: 'Installed' })
    const item = await screen.findByRole('listitem')
    await fireEvent.mouseEnter(item)
    vi.advanceTimersByTime(DEFAULT_TIMEOUT_MS * 2)
    expect(toasts.items).toHaveLength(1)
    await fireEvent.mouseLeave(item)
    vi.advanceTimersByTime(DEFAULT_TIMEOUT_MS)
    expect(toasts.items).toHaveLength(0)
    await tick()
    expect(within(screen.getByRole('region')).queryByRole('listitem')).toBeNull()
  })
})
