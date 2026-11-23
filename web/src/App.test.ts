import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import App from './App.svelte'
import { router } from './lib/router.svelte'
import { connection, toasts } from './lib/stores'
import { FakeSocket, instanceView, live, project, projectsFetch } from './test-fixtures'

const PROJECTS = [
  project('shop', [
    instanceView('main', { live: live('shop', 'main', { state: 'running', pid: 7 }) }),
    instanceView('test'),
  ]),
  project('crm'),
]

let sockets: FakeSocket[] = []

function renderApp(body: unknown = PROJECTS, status = 200) {
  vi.stubGlobal('fetch', vi.fn(projectsFetch(body, status)))
  sockets = []
  vi.stubGlobal(
    'WebSocket',
    class extends FakeSocket {
      constructor(url: string) {
        super(url)
        sockets.push(this)
      }
    },
  )
  return render(App)
}

function socket(): FakeSocket {
  const last = sockets.at(-1)
  if (!last) throw new Error('no WebSocket opened')
  return last
}

const sidebar = () => within(screen.getByTestId('sidebar'))

describe('App shell', () => {
  beforeEach(() => router.navigate('/', { replace: true }))
  afterEach(() => {
    toasts.clear()
    vi.unstubAllGlobals()
  })

  it('lists projects with their instances and status', async () => {
    renderApp()
    const nav = sidebar()
    expect(await nav.findByRole('link', { name: /shop/ })).toHaveTextContent('1/2')
    expect(nav.getByRole('link', { name: 'Running main' })).toHaveAttribute(
      'href',
      '/projects/shop/instances/main',
    )
    expect(nav.getByRole('link', { name: 'Stopped test' })).toBeInTheDocument()
    expect(nav.getByRole('link', { name: 'crm' })).toBeInTheDocument()
    expect(screen.getByRole('heading', { level: 1, name: 'Dashboard' })).toBeInTheDocument()
    expect(screen.getByText(/1 of 2 instances running/)).toBeInTheDocument()
  })

  it('navigates in-app and marks the current page', async () => {
    renderApp()
    const link = await sidebar().findByRole('link', { name: 'Running main' })
    await fireEvent.click(link)
    expect(window.location.pathname).toBe('/projects/shop/instances/main')
    expect(link).toHaveAttribute('aria-current', 'page')
    expect(screen.getByRole('heading', { level: 1, name: 'main' })).toBeInTheDocument()
    expect(document.title).toBe('shop/main · OdHive')

    const crumbs = within(screen.getByRole('navigation', { name: 'Breadcrumb' }))
    await fireEvent.click(crumbs.getByRole('link', { name: 'shop' }))
    expect(screen.getByRole('heading', { level: 1, name: 'shop' })).toBeInTheDocument()
  })

  it('shows an empty state and a sign-in hint', async () => {
    const { unmount } = renderApp([])
    expect(await sidebar().findByText('No projects yet.')).toBeInTheDocument()
    expect(screen.getByText('No projects yet')).toBeInTheDocument()
    unmount()

    renderApp({ error: 'unauthorized', message: 'sign in' }, 401)
    expect(await sidebar().findByRole('alert')).toHaveTextContent('Not signed in')
  })

  it('renders unknown paths as not found', () => {
    router.navigate('/somewhere/else')
    renderApp()
    expect(screen.getByRole('heading', { name: 'Page not found' })).toBeInTheDocument()
  })

  it('opens the navigation drawer, traps focus and closes on Escape', async () => {
    renderApp()
    const menu = screen.getByRole('button', { name: 'Open navigation' })
    expect(menu).toHaveAttribute('aria-expanded', 'false')
    await fireEvent.click(menu)

    const drawer = screen.getByRole('dialog', { name: 'Navigation' })
    expect(menu).toHaveAttribute('aria-expanded', 'true')
    await waitFor(() => expect(drawer.contains(document.activeElement)).toBe(true))

    // Shift+Tab from the first element wraps to the last one.
    await fireEvent.keyDown(window, { key: 'Tab', shiftKey: true })
    const links = within(drawer).getAllByRole('link')
    expect(document.activeElement).toBe(links[links.length - 1])

    await fireEvent.keyDown(window, { key: 'Escape' })
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(document.activeElement).toBe(menu)
  })

  it('closes the drawer after following a link', async () => {
    renderApp()
    await fireEvent.click(screen.getByRole('button', { name: 'Open navigation' }))
    const drawer = screen.getByRole('dialog', { name: 'Navigation' })
    await fireEvent.click(await within(drawer).findByRole('link', { name: 'crm' }))
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(window.location.pathname).toBe('/projects/crm')
  })

  it('switches the theme from the top bar', async () => {
    renderApp()
    const toggle = screen.getByRole('button', { name: /change theme/ })
    await fireEvent.click(toggle) // system → light
    expect(document.documentElement.dataset.theme).toBe('light')
    await fireEvent.click(toggle) // light → dark
    expect(document.documentElement.dataset.theme).toBe('dark')
    expect(toggle).toHaveAccessibleName('Dark theme (change theme)')
    await fireEvent.click(toggle) // back to system
  })

  it('keeps status live over the WebSocket and shows toasts', async () => {
    renderApp()
    const nav = sidebar()
    await nav.findByRole('link', { name: 'Stopped test' })
    expect(socket().url).toBe(`ws://${window.location.host}/api/v1/ws`)
    socket().open()
    expect(socket().sent).toEqual([{ op: 'subscribe_events' }, { op: 'subscribe_jobs' }])

    socket().receive({
      type: 'events',
      events: [
        { project: 'shop', instance: 'main', event: 'status', status: { state: 'crashed', exit: { code: 3, port_in_use: false, tail: '' } } },
      ],
    })
    expect(await nav.findByRole('link', { name: 'Crashed main' })).toBeInTheDocument()
    expect(await screen.findByRole('alert')).toHaveTextContent('shop/main crashed')
    expect(nav.getByRole('link', { name: /shop/ })).toHaveTextContent('0/2')
  })

  it('shows when the live connection is lost', async () => {
    renderApp()
    socket().open()
    socket().drop()
    expect(await screen.findByText('Reconnecting…')).toBeInTheDocument()
    connection.stop()
  })
})
