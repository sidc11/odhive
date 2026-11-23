// Light/dark theme: the user's choice (or the system preference) applied as
// `data-theme` on <html>. public/theme.js applies it before the first paint.

export type ThemeChoice = 'system' | 'light' | 'dark'
export type ResolvedTheme = 'light' | 'dark'

export const THEME_KEY = 'odhive.theme'
const CHOICES: readonly ThemeChoice[] = ['system', 'light', 'dark']
const DARK_QUERY = '(prefers-color-scheme: dark)'

function isChoice(value: unknown): value is ThemeChoice {
  return typeof value === 'string' && (CHOICES as readonly string[]).includes(value)
}

function readChoice(): ThemeChoice {
  try {
    const saved = window.localStorage.getItem(THEME_KEY)
    return isChoice(saved) ? saved : 'system'
  } catch {
    return 'system'
  }
}

function darkQuery(): MediaQueryList | null {
  return typeof window.matchMedia === 'function' ? window.matchMedia(DARK_QUERY) : null
}

export class Theme {
  choice = $state<ThemeChoice>('system')
  systemDark = $state(false)
  resolved: ResolvedTheme = $derived(
    this.choice === 'system' ? (this.systemDark ? 'dark' : 'light') : this.choice,
  )

  constructor() {
    this.choice = readChoice()
    const query = darkQuery()
    if (query) {
      this.systemDark = query.matches
      query.addEventListener('change', (event) => {
        this.systemDark = event.matches
        this.apply()
      })
    }
    this.apply()
  }

  set(choice: ThemeChoice): void {
    this.choice = choice
    try {
      if (choice === 'system') window.localStorage.removeItem(THEME_KEY)
      else window.localStorage.setItem(THEME_KEY, choice)
    } catch {
      // Not saved; the choice still holds for this page.
    }
    this.apply()
  }

  /** system → light → dark → system. */
  cycle(): void {
    const next = CHOICES[(CHOICES.indexOf(this.choice) + 1) % CHOICES.length]
    this.set(next ?? 'system')
  }

  private apply(): void {
    document.documentElement.dataset.theme = this.resolved
  }
}

export const theme = new Theme()
