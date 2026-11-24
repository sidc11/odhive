import { fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { tick } from 'svelte'
import { describe, expect, it } from 'vitest'
import LogView from './LogView.svelte'
import { LogStream } from './stream.svelte'
import { FakeConnection, msg, record, records } from './test-fixtures'

function setup() {
  const connection = new FakeConnection()
  const stream = new LogStream(connection, 'shop', 'main')
  stream.start()
  render(LogView, { stream, label: 'Log of shop/main' })
  const scroller = screen.getByRole('region', { name: 'Log of shop/main' })
  return { connection, stream, scroller }
}

/**
 * Gives the scroller a size and a working `scrollTop` (jsdom has no
 * layout), then scrolls it to `top`.
 */
function scrollTo(scroller: HTMLElement, top: number, { height = 200, content = 2000 } = {}): void {
  let position = 0
  Object.defineProperty(scroller, 'clientHeight', { configurable: true, value: height })
  Object.defineProperty(scroller, 'scrollHeight', { configurable: true, value: content })
  Object.defineProperty(scroller, 'scrollTop', {
    configurable: true,
    get: () => position,
    set: (value: number) => {
      position = Math.max(0, Math.min(value, content - height))
    },
  })
  scroller.scrollTop = top
  void fireEvent.scroll(scroller)
}

describe('LogView', () => {
  it('says what it waits for while empty', async () => {
    const { connection } = setup()
    expect(screen.getByText('Connecting to the log…')).toBeInTheDocument()
    connection.receive(msg.subscribed(1, false))
    expect(await screen.findByText(/No log yet/)).toBeInTheDocument()
    connection.receive(msg.subscribed(1, true))
    expect(await screen.findByText('Waiting for log output…')).toBeInTheDocument()
  })

  it('renders entries with level, logger and message', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs([record(1, 'Modules loaded.', { level: 'warning', db: 'shop_main' })]))
    await tick()
    const row = scroller.querySelector('[data-seq="1"]')
    expect(row?.textContent).toMatch(/^\d\d:\d\d:\d\d\.\d{3} +WARN +shop_main odoo\.modules\.loading: Modules loaded\.$/)
    expect(screen.queryByText('Waiting for log output…')).toBeNull()
  })

  it('renders markup in log lines as text', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs([record(1, '<script>alert(1)</script><img src=x onerror="alert(2)">')]))
    await tick()
    expect(scroller.querySelector('script, img')).toBeNull()
    expect(scroller.textContent).toContain('<script>alert(1)</script><img src=x onerror="alert(2)">')
  })

  it('renders only the rows near the viewport', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 5000)))
    await tick()
    const rendered = scroller.querySelectorAll('[data-seq]').length
    expect(rendered).toBeGreaterThan(0)
    expect(rendered).toBeLessThan(100)
  })

  it('marks gaps and restarts', async () => {
    const { connection } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 1)))
    connection.receive(msg.gap(1234))
    connection.receive(msg.reset())
    connection.receive(msg.logs(records(1, 1)))
    expect(await screen.findByText(/1,234 entries dropped from memory/)).toBeInTheDocument()
    expect(screen.getByText('── Log started over ──')).toBeInTheDocument()
  })

  it('pauses when scrolled up and jumps back to the latest entry', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 100)))
    await tick()
    expect(screen.queryByRole('button', { name: /Jump to latest/ })).toBeNull()

    scrollTo(scroller, 500)
    const jump = await screen.findByRole('button', { name: 'Jump to latest' })
    connection.receive(msg.logs(records(101, 103)))
    await tick()
    expect(jump).toHaveAccessibleName('Jump to latest (3 new)')

    await fireEvent.click(jump)
    expect(scroller.scrollTop).toBe(1800)
    expect(screen.queryByRole('button', { name: /Jump to latest/ })).toBeNull()
    expect(scroller).toHaveFocus()
  })

  it('follows again when scrolled back to the end', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 100)))
    await tick()
    scrollTo(scroller, 0)
    expect(await screen.findByRole('button', { name: 'Jump to latest' })).toBeInTheDocument()
    scrollTo(scroller, 1800)
    await tick()
    expect(screen.queryByRole('button', { name: /Jump to latest/ })).toBeNull()
  })

  it('pauses on wheel up even while at the end', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs(records(1, 100)))
    await tick()
    scrollTo(scroller, 1800)
    await fireEvent.wheel(scroller, { deltaY: -40 })
    expect(await screen.findByRole('button', { name: 'Jump to latest' })).toBeInTheDocument()
  })

  it('hides levels whose chip is off', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(
      msg.logs([record(1, 'boot', { level: 'debug' }), record(2, 'ready'), record(3, 'boom', { level: 'error' })]),
    )
    await tick()
    const debug = screen.getByRole('button', { name: 'DEBUG' })
    expect(debug).toHaveAttribute('aria-pressed', 'true')
    await fireEvent.click(debug)
    await fireEvent.click(screen.getByRole('button', { name: 'INFO' }))
    expect(debug).toHaveAttribute('aria-pressed', 'false')
    expect([...scroller.querySelectorAll('[data-seq]')].map((row) => row.getAttribute('data-seq'))).toEqual(['3'])

    await fireEvent.click(screen.getByRole('button', { name: 'ERROR' }))
    expect(await screen.findByText('No entries match the level and logger filters.')).toBeInTheDocument()
    // New entries go through the filter too.
    connection.receive(msg.logs([record(4, 'later', { level: 'critical' })]))
    await tick()
    expect(scroller.querySelector('[data-seq="4"]')).not.toBeNull()
  })

  it('filters by logger after a typing pause', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs([record(1, 'a', { logger: 'werkzeug' }), record(2, 'b')]))
    await tick()
    await fireEvent.input(screen.getByRole('textbox', { name: 'Logger' }), { target: { value: '-werkzeug' } })
    await waitFor(() => expect(scroller.querySelector('[data-seq="1"]')).toBeNull())
    expect(scroller.querySelector('[data-seq="2"]')).not.toBeNull()
  })

  it('highlights search matches and steps through them', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs([record(1, 'foo one'), record(2, 'bar'), record(3, 'FOO two foo')]))
    await tick()
    const search = screen.getByRole('searchbox', { name: 'Search the log' })
    await fireEvent.input(search, { target: { value: 'foo' } })
    const status = screen.getByTestId('log-search-status')
    await waitFor(() => expect(status).toHaveTextContent('2 matches'))
    expect([...scroller.querySelectorAll('mark')].map((mark) => mark.textContent)).toEqual(['foo', 'FOO', 'foo'])

    await fireEvent.keyDown(search, { key: 'Enter' })
    expect(status).toHaveTextContent('1 of 2')
    expect(scroller.querySelector('[data-current]')?.getAttribute('data-seq')).toBe('1')
    await fireEvent.click(screen.getByRole('button', { name: 'Next match' }))
    expect(status).toHaveTextContent('2 of 2')
    await fireEvent.keyDown(search, { key: 'Enter', shiftKey: true })
    expect(status).toHaveTextContent('1 of 2')
    // Navigating pauses following.
    expect(screen.getByRole('button', { name: /Jump to latest/ })).toBeInTheDocument()

    // Match case narrows it down; matches in new rows count at once.
    await fireEvent.click(screen.getByRole('button', { name: 'Match case' }))
    await waitFor(() => expect(status).toHaveTextContent('2 matches'))
    connection.receive(msg.logs([record(4, 'foo again')]))
    await waitFor(() => expect(status).toHaveTextContent('3 matches'))

    await fireEvent.keyDown(search, { key: 'Escape' })
    await waitFor(() => expect(scroller.querySelector('mark')).toBeNull())
    expect(status).toHaveTextContent('')
  })

  it('reports an invalid regular expression', async () => {
    const { connection } = setup()
    connection.receive(msg.subscribed(1))
    await fireEvent.click(screen.getByRole('button', { name: 'Regular expression' }))
    const search = screen.getByRole('searchbox', { name: 'Search the log' })
    await fireEvent.input(search, { target: { value: '(oops' } })
    expect(await screen.findByRole('alert')).toHaveTextContent(/Invalid search: .*unterminated group/i)
    expect(search).toHaveAttribute('aria-invalid', 'true')
    expect(screen.getByRole('button', { name: 'Next match' })).toBeDisabled()
  })

  it('searches with Ctrl+F inside the log', async () => {
    const { scroller } = setup()
    scroller.focus()
    await fireEvent.keyDown(scroller, { key: 'f', ctrlKey: true })
    expect(screen.getByRole('searchbox', { name: 'Search the log' })).toHaveFocus()
  })

  it('renders markup in highlighted matches as text', async () => {
    const { connection, scroller } = setup()
    connection.receive(msg.subscribed(1))
    connection.receive(msg.logs([record(1, '<img src=x onerror="alert(1)"> img')]))
    await tick()
    await fireEvent.input(screen.getByRole('searchbox', { name: 'Search the log' }), { target: { value: 'img' } })
    await waitFor(() => expect(scroller.querySelectorAll('mark')).toHaveLength(2))
    expect(scroller.querySelector('img')).toBeNull()
  })
})
