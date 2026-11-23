// Short notifications in the corner. Errors and toasts with actions stay
// until dismissed; the rest go after a while (paused while hovered or
// focused, so they can be read).
export type ToastKind = 'info' | 'success' | 'warning' | 'error'

export interface ToastAction {
  label: string
  run: () => void | Promise<void>
}

export interface ToastInput {
  kind: ToastKind
  title: string
  body?: string
  actions?: ToastAction[]
  /** A new toast with the same key replaces the old one. */
  key?: string
  /** Milliseconds; `null` = until dismissed. Defaults by kind. */
  timeout?: number | null
}

export interface Toast extends ToastInput {
  id: number
  timeout: number | null
}

export const DEFAULT_TIMEOUT_MS = 6000
/** More are dropped, oldest first. */
export const MAX_TOASTS = 5

interface Timer {
  handle: ReturnType<typeof setTimeout> | null
  remaining: number
  startedAt: number
}

export class ToastStore {
  items = $state<Toast[]>([])
  private nextId = 1
  // Timers, not UI state: a plain map on purpose.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  private readonly timers = new Map<number, Timer>()

  push(input: ToastInput): number {
    const sticky = input.kind === 'error' || (input.actions?.length ?? 0) > 0
    const timeout = input.timeout === undefined ? (sticky ? null : DEFAULT_TIMEOUT_MS) : input.timeout
    const toast: Toast = { ...input, id: this.nextId++, timeout }
    if (input.key !== undefined) this.dismissKey(input.key)
    this.items.push(toast)
    while (this.items.length > MAX_TOASTS) {
      const oldest = this.items[0]
      if (oldest) this.dismiss(oldest.id)
    }
    if (timeout !== null) {
      this.timers.set(toast.id, { handle: null, remaining: timeout, startedAt: 0 })
      this.resume(toast.id)
    }
    return toast.id
  }

  dismiss(id: number): void {
    const timer = this.timers.get(id)
    if (timer?.handle) clearTimeout(timer.handle)
    this.timers.delete(id)
    const index = this.items.findIndex((toast) => toast.id === id)
    if (index !== -1) this.items.splice(index, 1)
  }

  dismissKey(key: string): void {
    for (const toast of this.items.filter((candidate) => candidate.key === key)) this.dismiss(toast.id)
  }

  /** Stops the countdown (pointer over the toast, or focus in it). */
  pause(id: number): void {
    const timer = this.timers.get(id)
    if (!timer?.handle) return
    clearTimeout(timer.handle)
    timer.handle = null
    timer.remaining -= Date.now() - timer.startedAt
  }

  resume(id: number): void {
    const timer = this.timers.get(id)
    if (!timer || timer.handle) return
    timer.startedAt = Date.now()
    timer.handle = setTimeout(() => this.dismiss(id), Math.max(timer.remaining, 0))
  }

  clear(): void {
    for (const toast of [...this.items]) this.dismiss(toast.id)
  }
}
