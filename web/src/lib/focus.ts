// Keyboard focus helpers for modal dialogs.

const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(', ')

/** Elements in `container` reachable with Tab, in document order. */
export function focusables(container: HTMLElement): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>(FOCUSABLE)]
}

/**
 * On Tab, keeps focus inside `container` (wrapping at both ends). Returns
 * whether the event was handled.
 */
export function trapTab(event: KeyboardEvent, container: HTMLElement): boolean {
  if (event.key !== 'Tab') return false
  const all = focusables(container)
  const first = all[0]
  const last = all[all.length - 1]
  if (!first || !last) {
    event.preventDefault()
    return true
  }
  const active = document.activeElement
  if (event.shiftKey && (active === first || !container.contains(active))) {
    event.preventDefault()
    last.focus()
    return true
  }
  if (!event.shiftKey && (active === last || !container.contains(active))) {
    event.preventDefault()
    first.focus()
    return true
  }
  return false
}
