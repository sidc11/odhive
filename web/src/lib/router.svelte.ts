// Client-side routes on the History API. The server answers every path
// without a file extension with the app shell, so deep links reload fine.

export type Route =
  | { name: 'dashboard' }
  | { name: 'new_project' }
  | { name: 'project'; project: string }
  | { name: 'instance'; project: string; instance: string }
  | { name: 'not_found'; path: string }

function segment(raw: string): string | null {
  try {
    const value = decodeURIComponent(raw)
    return value === '' || value.includes('/') ? null : value
  } catch {
    return null
  }
}

export function parseRoute(path: string): Route {
  const parts = path.split('/').filter((part) => part !== '')
  const notFound: Route = { name: 'not_found', path }
  if (parts.length === 0) return { name: 'dashboard' }
  // Not below /projects/: "new" is a valid project name.
  if (parts.length === 1 && parts[0] === 'new') return { name: 'new_project' }
  if (parts[0] !== 'projects') return notFound

  const project = parts[1] === undefined ? null : segment(parts[1])
  if (project === null) return notFound
  if (parts.length === 2) return { name: 'project', project }

  const instance = parts[3] === undefined ? null : segment(parts[3])
  if (parts.length === 4 && parts[2] === 'instances' && instance !== null) {
    return { name: 'instance', project, instance }
  }
  return notFound
}

export function routeHref(route: Exclude<Route, { name: 'not_found' }>): string {
  switch (route.name) {
    case 'dashboard':
      return '/'
    case 'new_project':
      return '/new'
    case 'project':
      return `/projects/${encodeURIComponent(route.project)}`
    case 'instance':
      return `/projects/${encodeURIComponent(route.project)}/instances/${encodeURIComponent(route.instance)}`
  }
}

export class Router {
  path = $state('/')
  route: Route = $derived(parseRoute(this.path))

  constructor() {
    this.path = window.location.pathname
    window.addEventListener('popstate', () => {
      this.path = window.location.pathname
    })
  }

  navigate(href: string, options: { replace?: boolean } = {}): void {
    // A throwaway parse, never stored or mutated, so it needs no reactivity.
    // eslint-disable-next-line svelte/prefer-svelte-reactivity
    const url = new URL(href, window.location.origin)
    if (url.origin !== window.location.origin) return
    if (options.replace) window.history.replaceState(null, '', url)
    else if (url.pathname !== this.path) window.history.pushState(null, '', url)
    this.path = url.pathname
  }

  /** Click handler for in-app links: plain left clicks stay in the app. */
  link = (event: MouseEvent): void => {
    if (event.defaultPrevented || event.button !== 0) return
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return
    const anchor = event.currentTarget
    if (!(anchor instanceof HTMLAnchorElement) || anchor.target === '_blank') return
    event.preventDefault()
    this.navigate(anchor.href)
  }
}

export const router = new Router()
