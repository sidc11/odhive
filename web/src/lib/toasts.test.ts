import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { DEFAULT_TIMEOUT_MS, MAX_TOASTS, ToastStore } from './toasts.svelte'

describe('ToastStore', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('dismisses info toasts after the timeout; errors and actions stay', () => {
    const toasts = new ToastStore()
    toasts.push({ kind: 'info', title: 'a' })
    toasts.push({ kind: 'error', title: 'b' })
    toasts.push({ kind: 'warning', title: 'c', actions: [{ label: 'Go', run: () => {} }] })
    vi.advanceTimersByTime(DEFAULT_TIMEOUT_MS)
    expect(toasts.items.map((t) => t.title)).toEqual(['b', 'c'])
  })

  it('pauses the countdown and resumes with the time left', () => {
    const toasts = new ToastStore()
    const id = toasts.push({ kind: 'success', title: 'a', timeout: 1000 })
    vi.advanceTimersByTime(600)
    toasts.pause(id)
    vi.advanceTimersByTime(5000)
    expect(toasts.items).toHaveLength(1)
    toasts.resume(id)
    vi.advanceTimersByTime(399)
    expect(toasts.items).toHaveLength(1)
    vi.advanceTimersByTime(1)
    expect(toasts.items).toHaveLength(0)
  })

  it('replaces a toast with the same key', () => {
    const toasts = new ToastStore()
    toasts.push({ kind: 'info', key: 'k', title: 'old' })
    toasts.push({ kind: 'info', key: 'other', title: 'keep' })
    toasts.push({ kind: 'success', key: 'k', title: 'new' })
    expect(toasts.items.map((t) => t.title)).toEqual(['keep', 'new'])
    toasts.dismissKey('k')
    expect(toasts.items.map((t) => t.title)).toEqual(['keep'])
  })

  it('keeps at most MAX_TOASTS, dropping the oldest', () => {
    const toasts = new ToastStore()
    for (let i = 0; i < MAX_TOASTS + 2; i++) toasts.push({ kind: 'error', title: `t${i}` })
    expect(toasts.items).toHaveLength(MAX_TOASTS)
    expect(toasts.items[0]?.title).toBe('t2')
    toasts.clear()
    expect(toasts.items).toHaveLength(0)
  })
})
