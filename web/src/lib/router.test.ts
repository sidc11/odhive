import { afterEach, describe, expect, it } from 'vitest'
import { parseRoute, routeHref, Router } from './router.svelte'

describe('parseRoute', () => {
  it('maps the known paths', () => {
    expect(parseRoute('/')).toEqual({ name: 'dashboard' })
    expect(parseRoute('')).toEqual({ name: 'dashboard' })
    expect(parseRoute('/new')).toEqual({ name: 'new_project' })
    expect(parseRoute('/environment')).toEqual({ name: 'environment' })
    expect(parseRoute('/environment/x')).toEqual({ name: 'not_found', path: '/environment/x' })
    // A project may be called "new".
    expect(parseRoute('/projects/new')).toEqual({ name: 'project', project: 'new' })
    expect(parseRoute('/projects/shop')).toEqual({ name: 'project', project: 'shop' })
    expect(parseRoute('/projects/shop/')).toEqual({ name: 'project', project: 'shop' })
    expect(parseRoute('/projects/shop/instances/main')).toEqual({
      name: 'instance',
      project: 'shop',
      instance: 'main',
    })
    expect(parseRoute('/projects/shop/config')).toEqual({ name: 'config', project: 'shop' })
  })

  it('decodes segments', () => {
    expect(parseRoute('/projects/my%20shop')).toEqual({ name: 'project', project: 'my shop' })
  })

  it('rejects unknown, malformed and smuggled paths', () => {
    for (const path of [
      '/nope',
      '/new/x',
      '/projects',
      '/projects/shop/instances',
      '/projects/shop/other/main',
      '/projects/shop/config/x',
      '/projects/shop/configs',
      '/projects/shop/instances/main/extra',
      '/projects/%E0%A4%A', // invalid percent-encoding
      '/projects/a%2Fb', // encoded slash must not become a second segment
    ]) {
      expect(parseRoute(path)).toEqual({ name: 'not_found', path })
    }
  })

  it('round-trips through routeHref', () => {
    const routes = [
      { name: 'dashboard' },
      { name: 'new_project' },
      { name: 'environment' },
      { name: 'project', project: 'a b?#' },
      { name: 'config', project: 'a b?#' },
      { name: 'instance', project: 'shop', instance: 'main/x' },
    ] as const
    for (const route of routes) {
      const parsed = parseRoute(routeHref(route))
      if (route.name === 'instance' && route.instance.includes('/')) {
        // routeHref encodes the slash; parseRoute refuses it on the way back.
        expect(parsed.name).toBe('not_found')
      } else {
        expect(parsed).toEqual(route)
      }
    }
  })
})

describe('log split view routes', () => {
  it('maps /logs and up to four project/instance pairs', () => {
    expect(parseRoute('/logs')).toEqual({ name: 'logs', panes: [] })
    expect(parseRoute('/logs/shop/main/')).toEqual({ name: 'logs', panes: [{ project: 'shop', instance: 'main' }] })
    expect(parseRoute('/logs/shop/main/crm/test/shop/main/a/b')).toEqual({
      name: 'logs',
      panes: [
        { project: 'shop', instance: 'main' },
        { project: 'crm', instance: 'test' },
        { project: 'shop', instance: 'main' },
        { project: 'a', instance: 'b' },
      ],
    })
  })

  it('rejects odd, too many and malformed segments', () => {
    for (const path of ['/logs/shop', '/logs/shop/main/crm', '/logs/a/1/b/2/c/3/d/4/e/5', '/logs/a%2Fb/main', '/logs/%E0/x']) {
      expect(parseRoute(path)).toEqual({ name: 'not_found', path })
    }
  })

  it('round-trips through routeHref', () => {
    for (const panes of [[], [{ project: 'shop', instance: 'main' }], [{ project: 'a b', instance: 'x' }, { project: 'c', instance: 'd' }]]) {
      expect(parseRoute(routeHref({ name: 'logs', panes }))).toEqual({ name: 'logs', panes })
    }
    expect(routeHref({ name: 'logs', panes: [] })).toBe('/logs')
  })
})

describe('Router', () => {
  afterEach(() => window.history.replaceState(null, '', '/'))

  it('pushes history entries and follows popstate', () => {
    const router = new Router()
    const before = window.history.length
    router.navigate('/projects/shop')
    expect(router.route).toEqual({ name: 'project', project: 'shop' })
    expect(window.location.pathname).toBe('/projects/shop')
    expect(window.history.length).toBe(before + 1)

    router.navigate('/projects/shop') // same path: no duplicate entry
    expect(window.history.length).toBe(before + 1)

    window.history.replaceState(null, '', '/')
    window.dispatchEvent(new PopStateEvent('popstate'))
    expect(router.route).toEqual({ name: 'dashboard' })
  })

  it('ignores other origins', () => {
    const router = new Router()
    router.navigate('https://example.com/projects/x')
    expect(router.path).toBe('/')
  })

  it('handles plain clicks and leaves modified clicks to the browser', () => {
    const router = new Router()
    const anchor = document.createElement('a')
    anchor.href = '/projects/shop'
    anchor.addEventListener('click', router.link)
    document.body.append(anchor)

    const modified = new MouseEvent('click', { bubbles: true, cancelable: true, ctrlKey: true })
    anchor.dispatchEvent(modified)
    expect(modified.defaultPrevented).toBe(false)
    expect(router.path).toBe('/')

    const plain = new MouseEvent('click', { bubbles: true, cancelable: true })
    anchor.dispatchEvent(plain)
    expect(plain.defaultPrevented).toBe(true)
    expect(router.path).toBe('/projects/shop')
    anchor.remove()
  })
})
