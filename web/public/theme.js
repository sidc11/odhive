// Applies the saved theme before the first paint, so a dark page never
// flashes light. A separate file because the CSP forbids inline scripts.
// Keep the key and values in sync with src/lib/theme.svelte.ts.
;(function () {
  var choice = null
  try {
    choice = window.localStorage.getItem('odhive.theme')
  } catch {
    // Storage can be blocked; fall back to the system preference.
  }
  var dark =
    choice === 'dark' ||
    (choice !== 'light' &&
      typeof window.matchMedia === 'function' &&
      window.matchMedia('(prefers-color-scheme: dark)').matches)
  document.documentElement.dataset.theme = dark ? 'dark' : 'light'
})()
