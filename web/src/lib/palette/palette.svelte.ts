// Whether the command palette is open, and where focus goes back to.

/** Ctrl+K, or Cmd+K on macOS; no other modifiers. */
export function isPaletteShortcut(event: KeyboardEvent): boolean {
  if (event.key.toLowerCase() !== 'k' || event.shiftKey || event.altKey) return false
  return event.ctrlKey !== event.metaKey
}

export class PaletteStore {
  open = $state(false)
  private returnFocus: HTMLElement | null = null

  show(): void {
    if (this.open) return
    const active = document.activeElement
    this.returnFocus = active instanceof HTMLElement && active !== document.body ? active : null
    this.open = true
  }

  /** Closes and gives focus back to what had it before opening. */
  hide(): void {
    if (!this.open) return
    this.open = false
    const target = this.returnFocus
    this.returnFocus = null
    if (target?.isConnected) target.focus()
  }

  toggle(): void {
    if (this.open) this.hide()
    else this.show()
  }
}

/** How the shortcut is written on this machine. */
export function shortcutLabel(platform = navigator.userAgent): string {
  return /Mac|iPhone|iPad/.test(platform) ? '⌘K' : 'Ctrl K'
}
