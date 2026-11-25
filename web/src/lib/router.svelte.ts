// Client-side routes on the History API. The server answers every path
// without a file extension with the app shell, so deep links reload fine.

export type Route =
  | { name: 'dashboard' }
  | { name: 'new_project' }
  | { name: 'environment' }
  | { name: 'project'; project: string }
  | { name: 'instance'; project: string; instance: string }
  | { name: 'config'; project: string }
  | { name: 'logs'; panes: LogPane[] }
  | { name: 'not_found'; path: string }

/** One log of the split view. */
export interface LogPane {
  project: string
  instance: string
}

/** Most logs the split view shows side by side. */
export const MAX_PANES = 4

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
  if (parts.length === 1 && parts[0] === 'environment') return { name: 'environment' }
  // /logs/<project>/<instance>/<project>/<instance>…
  if (parts[0] === 'logs') {
    const rest = parts.slice(1).map(segment)
    if (rest.length % 2 !== 0 || rest.length > MAX_PANES * 2) return notFound
    const panes: LogPane[] = []
    for (let i = 0; i < rest.length; i += 2) {
      const project = rest[i]
      const instance = rest[i + 1]
      if (project == null || instance == null) return notFound
      panes.push({ project, instance })
    }
    return { name: 'logs', panes }
  }
  if (parts[0] !== 'projects') return notFound

  const project = parts[1] === undefined ? null : segment(parts[1])
  if (project === null) return notFound
  if (parts.length === 2) return { name: 'project', project }
  if (parts.length === 3 && parts[2] === 'config') return { name: 'config', project }

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
    case 'environment':
      return '/environment'
    case 'project':
      return `/projects/${encodeURIComponent(route.project)}`
    case 'instance':
      return `/projects/${encodeURIComponent(route.project)}/instances/${encodeURIComponent(route.instance)}`
    case 'config':
      return `/projects/${encodeURIComponent(route.project)}/config`
    case 'logs':
      return ['/logs', ...route.panes.flatMap((pane) => [pane.project, pane.instance].map(encodeURIComponent))].join(
        '/',
      )
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
