import { fireEvent, render, screen } from '@testing-library/svelte'
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
})
