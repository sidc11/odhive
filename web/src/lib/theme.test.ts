import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { Theme, THEME_KEY } from './theme.svelte'

type Listener = (event: { matches: boolean }) => void

function fakeMedia(initialDark: boolean) {
  const listeners: Listener[] = []
  const query = {
    matches: initialDark,
    addEventListener: (_: string, listener: Listener) => listeners.push(listener),
  }
  vi.stubGlobal('matchMedia', () => query)
  return {
    setDark(dark: boolean) {
      query.matches = dark
      for (const listener of listeners) listener({ matches: dark })
    },
  }
}

const applied = () => document.documentElement.dataset.theme

describe('Theme', () => {
  beforeEach(() => window.localStorage.clear())
  afterEach(() => {
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  it('follows the system preference by default, live', () => {
    const media = fakeMedia(true)
    const theme = new Theme()
    expect(theme.choice).toBe('system')
    expect(applied()).toBe('dark')
    media.setDark(false)
    expect(theme.resolved).toBe('light')
    expect(applied()).toBe('light')
  })

  it('saves an explicit choice and forgets it for system', () => {
    fakeMedia(false)
    const theme = new Theme()
    theme.set('dark')
    expect(applied()).toBe('dark')
    expect(window.localStorage.getItem(THEME_KEY)).toBe('dark')
    expect(new Theme().choice).toBe('dark')

    theme.set('system')
    expect(window.localStorage.getItem(THEME_KEY)).toBeNull()
    expect(applied()).toBe('light')
  })

  it('cycles system → light → dark → system', () => {
    fakeMedia(false)
    const theme = new Theme()
    const seen = [theme.choice]
    for (let i = 0; i < 3; i++) {
      theme.cycle()
      seen.push(theme.choice)
    }
    expect(seen).toEqual(['system', 'light', 'dark', 'system'])
  })

  it('ignores garbage in storage and survives blocked storage', () => {
    fakeMedia(false)
    window.localStorage.setItem(THEME_KEY, 'purple')
    expect(new Theme().choice).toBe('system')

    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    const theme = new Theme()
    expect(theme.choice).toBe('system')
    theme.set('dark')
    expect(applied()).toBe('dark')
  })

  it('works without matchMedia', () => {
    vi.stubGlobal('matchMedia', undefined)
    expect(new Theme().resolved).toBe('light')
  })
})
