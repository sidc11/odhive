import { afterEach, describe, expect, it } from 'vitest'
import { parseRoute, routeHref, Router } from './router.svelte'

describe('parseRoute', () => {
  it('maps the known paths', () => {
    expect(parseRoute('/')).toEqual({ name: 'dashboard' })
    expect(parseRoute('')).toEqual({ name: 'dashboard' })
    expect(parseRoute('/projects/shop')).toEqual({ name: 'project', project: 'shop' })
    expect(parseRoute('/projects/shop/')).toEqual({ name: 'project', project: 'shop' })
    expect(parseRoute('/projects/shop/instances/main')).toEqual({
      name: 'instance',
      project: 'shop',
      instance: 'main',
    })
  })

  it('decodes segments', () => {
    expect(parseRoute('/projects/my%20shop')).toEqual({ name: 'project', project: 'my shop' })
  })

  it('rejects unknown, malformed and smuggled paths', () => {
    for (const path of [
      '/nope',
      '/projects',
      '/projects/shop/instances',
      '/projects/shop/other/main',
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
      { name: 'project', project: 'a b?#' },
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
