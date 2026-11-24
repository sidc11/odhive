import { afterEach, describe, expect, it } from 'vitest'
import { isPaletteShortcut, PaletteStore, shortcutLabel } from './palette.svelte'

const key = (init: KeyboardEventInit): KeyboardEvent => new KeyboardEvent('keydown', init)

afterEach(() => {
  document.body.innerHTML = ''
})

describe('isPaletteShortcut', () => {
  it('takes Ctrl+K and Cmd+K only', () => {
    expect(isPaletteShortcut(key({ key: 'k', ctrlKey: true }))).toBe(true)
    expect(isPaletteShortcut(key({ key: 'K', metaKey: true }))).toBe(true)
    expect(isPaletteShortcut(key({ key: 'k' }))).toBe(false)
    expect(isPaletteShortcut(key({ key: 'k', ctrlKey: true, shiftKey: true }))).toBe(false)
    expect(isPaletteShortcut(key({ key: 'k', ctrlKey: true, altKey: true }))).toBe(false)
    expect(isPaletteShortcut(key({ key: 'k', ctrlKey: true, metaKey: true }))).toBe(false)
    expect(isPaletteShortcut(key({ key: 'j', ctrlKey: true }))).toBe(false)
  })
})

describe('shortcutLabel', () => {
  it('follows the platform', () => {
    expect(shortcutLabel('Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)')).toBe('⌘K')
    expect(shortcutLabel('Mozilla/5.0 (X11; Linux x86_64)')).toBe('Ctrl K')
  })
})

describe('PaletteStore', () => {
  it('gives focus back to what had it when closed', () => {
    const button = document.createElement('button')
    document.body.append(button)
    button.focus()
    const palette = new PaletteStore()
    palette.show()
    expect(palette.open).toBe(true)
    document.body.focus()
    palette.hide()
    expect(palette.open).toBe(false)
    expect(document.activeElement).toBe(button)
  })

  it('toggles, and skips focus targets that are gone', () => {
    const button = document.createElement('button')
    document.body.append(button)
    button.focus()
    const palette = new PaletteStore()
    palette.toggle()
    button.remove()
    palette.toggle()
    expect(palette.open).toBe(false)
  })
})
