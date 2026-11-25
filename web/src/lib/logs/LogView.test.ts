import { fireEvent, render, screen, waitFor } from '@testing-library/svelte'
import { tick, type ComponentProps } from 'svelte'
import { describe, expect, it, vi } from 'vitest'
import { download } from './export'
import LogView from './LogView.svelte'
import { LogStream } from './stream.svelte'
import { FakeConnection, msg, record, records } from './test-fixtures'

vi.mock('./export', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./export')>()),
  download: vi.fn(),
}))

function setup(props: Partial<ComponentProps<typeof LogView>> = {}) {
  const connection = new FakeConnection()
  const stream = new LogStream(connection, 'shop', 'main')
  stream.start()
  render(LogView, { stream, label: 'Log of shop/main', ...props })
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

  describe('tracebacks', () => {
    const TRACE = [
      'Failed to load registry',
      'Traceback (most recent call last):',
      '  File "/home/me/shop/addons/sale_x/models/order.py", line 12, in <module>',
      '    import ldap',
      "ModuleNotFoundError: No module named 'ldap'",
    ].join('\n')
    const trace = (seq: number) => record(seq, TRACE, { level: 'error', exceptions: ["ModuleNotFoundError: No module named 'ldap'"] })

    it('are folded to one line until unfolded', async () => {
      const { connection, scroller } = setup()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([trace(1)]))
      await tick()
      const row = () => scroller.querySelector('[data-seq="1"]') as HTMLElement
      expect(row()).toHaveAttribute('data-folded')
      expect(row().textContent).toContain("Failed to load registry · ModuleNotFoundError: No module named 'ldap' · 4 more lines")
      expect(row().textContent).not.toContain('import ldap')
      const unfold = screen.getByRole('button', { name: 'Unfold traceback' })
      expect(unfold).toHaveAttribute('aria-expanded', 'false')
      await fireEvent.click(unfold)
      expect(row()).not.toHaveAttribute('data-folded')
      expect(row().textContent).toContain('    import ldap\n')
      expect(row().style.height).toBe('100px')
      const fold = screen.getByRole('button', { name: 'Fold traceback' })
      expect(fold).toHaveAttribute('aria-expanded', 'true')
      await fireEvent.click(fold)
      expect(row()).toHaveAttribute('data-folded')
      expect(row().style.height).toBe('20px')
    })

    it('copy the traceback exactly as logged', async () => {
      const writeText = vi.fn<(text: string) => Promise<void>>().mockResolvedValue(undefined)
      Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } })
      const { connection } = setup()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([trace(1)]))
      const copy = await screen.findByRole('button', { name: 'Copy traceback' })
      await fireEvent.click(copy)
      expect(writeText).toHaveBeenCalledWith(TRACE.slice(TRACE.indexOf('Traceback')))
      await waitFor(() => expect(copy).toHaveTextContent('Copied'))
      writeText.mockRejectedValueOnce(new Error('denied'))
      await fireEvent.click(copy)
      await waitFor(() => expect(copy).toHaveTextContent('Copy failed'))
    })

    it('link file:line to the editor when unfolded', async () => {
      const onopen = vi.fn()
      const { connection } = setup({ onopen })
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([trace(1)]))
      await fireEvent.click(await screen.findByRole('button', { name: 'Unfold traceback' }))
      const link = screen.getByRole('button', { name: '/home/me/shop/addons/sale_x/models/order.py' })
      expect(link).toHaveAttribute('title', 'Open /home/me/shop/addons/sale_x/models/order.py:12 in the editor')
      await fireEvent.click(link)
      expect(onopen).toHaveBeenCalledWith(expect.objectContaining({ path: '/home/me/shop/addons/sale_x/models/order.py', line: 12 }))
    })

    it('have no links without an opener', async () => {
      const { connection } = setup()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([trace(1)]))
      await fireEvent.click(await screen.findByRole('button', { name: 'Unfold traceback' }))
      expect(screen.queryByRole('button', { name: /order\.py/ })).toBeNull()
    })

    it('unfold when a search match in them is shown', async () => {
      const { connection, scroller } = setup()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([trace(1)]))
      await tick()
      const search = screen.getByRole('searchbox', { name: 'Search the log' })
      await fireEvent.input(search, { target: { value: 'import ldap' } })
      await waitFor(() => expect(screen.getByTestId('log-search-status')).toHaveTextContent('1 match'))
      await fireEvent.keyDown(search, { key: 'Enter' })
      await waitFor(() => expect(scroller.querySelector('[data-seq="1"]')).not.toHaveAttribute('data-folded'))
      expect(scroller.querySelector('mark')?.textContent).toBe('import ldap')
    })
  })

  describe('copy actions', () => {
    function clipboard() {
      const writeText = vi.fn<(text: string) => Promise<void>>().mockResolvedValue(undefined)
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: { writeText },
      })
      return writeText
    }

    function selectRows(scroller: HTMLElement, from: number, to: number): void {
      const start = scroller.querySelector(`[data-seq="${from}"]`)
      const end = scroller.querySelector(`[data-seq="${to}"]`)
      if (!start || !end) throw new Error('rows not rendered')
      const range = document.createRange()
      range.setStart(start, 0)
      range.setEnd(end, end.childNodes.length)
      const selection = document.getSelection()
      selection?.removeAllRanges()
      selection?.addRange(range)
      document.dispatchEvent(new Event('selectionchange'))
    }

    it('copies the entries on screen as log text', async () => {
      const writeText = clipboard()
      // The viewport is measured on mount (jsdom has no layout or ResizeObserver).
      const own = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'clientHeight')
      Object.defineProperty(HTMLElement.prototype, 'clientHeight', {
        configurable: true,
        get(this: HTMLElement) {
          return this.dataset['testid'] === 'log-scroller' ? 200 : 0
        },
      })
      let mounted: ReturnType<typeof setup>
      try {
        mounted = setup()
      } finally {
        if (own) Object.defineProperty(HTMLElement.prototype, 'clientHeight', own)
        else delete (HTMLElement.prototype as { clientHeight?: number }).clientHeight
      }
      const { connection, scroller } = mounted
      const copy = screen.getByRole('button', { name: 'Copy visible' })
      expect(copy).toBeDisabled()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs(records(1, 100)))
      await tick()
      // 20px lines, 200px tall: rows 11 to 20 are on screen.
      scrollTo(scroller, 200)
      await tick()
      await fireEvent.click(copy)
      const text = writeText.mock.calls[0]?.[0] ?? ''
      expect(text.split('\n')).toHaveLength(10)
      expect(text.split('\n')[0]).toBe('2026-11-24 09:00:00,000 4242 INFO ? odoo.modules.loading: entry 11')
      expect(text.split('\n')[9]).toMatch(/: entry 20$/)
      expect(await screen.findByRole('button', { name: 'Copied 10 entries' })).toBeInTheDocument()
    })

    it('copies whole entries the selection touches and pauses follow', async () => {
      const writeText = clipboard()
      const { connection, scroller } = setup()
      const copy = screen.getByRole('button', { name: 'Copy selection' })
      expect(copy).toBeDisabled()
      connection.receive(msg.subscribed(1))
      connection.receive(
        msg.logs([
          record(1),
          record(2, 'boom\nTraceback (most recent call last):\nKeyError: 1', {
            exceptions: ['KeyError: 1'],
          }),
          record(3),
        ]),
      )
      await tick()
      expect(scroller.querySelector('[data-seq="2"]')).toHaveAttribute('data-folded')
      expect(screen.queryByRole('button', { name: /Jump to latest/ })).toBeNull()
      selectRows(scroller, 2, 3)
      await tick()
      // Selecting pauses following, so new rows don't scroll the selection away.
      expect(screen.getByRole('button', { name: /Jump to latest/ })).toBeInTheDocument()
      expect(copy).toBeEnabled()
      await fireEvent.click(copy)
      // The folded traceback is copied in full.
      expect(writeText).toHaveBeenCalledWith(
        [
          '2026-11-24 09:00:00,000 4242 INFO ? odoo.modules.loading: boom',
          'Traceback (most recent call last):',
          'KeyError: 1',
          '2026-11-24 09:00:00,000 4242 INFO ? odoo.modules.loading: entry 3',
        ].join('\n'),
      )
      expect(await screen.findByRole('button', { name: 'Copied 2 entries' })).toBeInTheDocument()
      document.getSelection()?.removeAllRanges()
      document.dispatchEvent(new Event('selectionchange'))
    })

    it('says when the clipboard refuses', async () => {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: { writeText: vi.fn().mockRejectedValue(new Error('denied')) },
      })
      const { connection } = setup()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs(records(1, 3)))
      await tick()
      await fireEvent.click(screen.getByRole('button', { name: 'Copy visible' }))
      expect(await screen.findByRole('button', { name: 'Copy failed' })).toBeInTheDocument()
    })
  })

  describe('export', () => {
    const SOURCE = { project: 'shop', instance: 'main' }

    function downloaded(): { name: string; type: string; text: string } {
      const calls = vi.mocked(download).mock.calls
      const file = calls.at(-1)?.[0]
      if (!file) throw new Error('nothing downloaded')
      return file
    }

    it('is offered only with a source', () => {
      setup()
      expect(screen.queryByRole('button', { name: /^Export/ })).toBeNull()
    })

    it('downloads the entries the filters show as .log', async () => {
      vi.mocked(download).mockClear()
      const { connection } = setup({ source: SOURCE })
      const exportButton = screen.getByRole('button', { name: /^Export/ })
      expect(exportButton).toBeDisabled()
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([record(1, 'boot', { level: 'debug' }), record(2, 'ready'), record(3, 'later')]))
      await tick()
      await fireEvent.click(screen.getByRole('button', { name: 'DEBUG' }))
      expect(exportButton).toHaveAttribute('aria-expanded', 'false')
      await fireEvent.click(exportButton)
      expect(exportButton).toHaveAttribute('aria-expanded', 'true')
      // No search yet: nothing to narrow to.
      expect(screen.getByRole('checkbox', { name: 'Only search matches' })).toBeDisabled()
      await fireEvent.click(screen.getByRole('button', { name: /Download \.log/ }))

      const file = downloaded()
      expect(file.name).toMatch(/^shop-main-\d{8}-\d{6}\.log$/)
      expect(file.text).toBe(
        '2026-11-24 09:00:00,000 4242 INFO ? odoo.modules.loading: ready\n' +
          '2026-11-24 09:00:00,000 4242 INFO ? odoo.modules.loading: later\n',
      )
      expect(exportButton).toHaveAttribute('aria-expanded', 'false')
      expect(exportButton).toHaveFocus()
      expect(exportButton).toHaveAccessibleName(/Exported 2 entries/)
    })

    it('can narrow to search matches and records the filter in .json', async () => {
      vi.mocked(download).mockClear()
      const { connection } = setup({ source: SOURCE })
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs([record(1, 'sale order 7'), record(2, 'ready'), record(3, 'sale order 9')]))
      await tick()
      await fireEvent.input(screen.getByRole('searchbox', { name: 'Search the log' }), {
        target: { value: 'sale' },
      })
      await waitFor(() => expect(screen.getByTestId('log-search-status')).toHaveTextContent('2 matches'))
      await fireEvent.click(screen.getByRole('button', { name: /^Export/ }))
      const only = screen.getByRole('checkbox', { name: 'Only search matches' })
      expect(only).toBeEnabled()
      await fireEvent.click(only)
      await fireEvent.click(screen.getByRole('button', { name: /Download \.json/ }))

      const file = downloaded()
      expect(file.type).toBe('application/json;charset=utf-8')
      const parsed = JSON.parse(file.text) as {
        filter: { search: string; only_matching: boolean }
        entries: { seq: number }[]
      }
      expect(parsed.filter).toMatchObject({ search: 'sale', only_matching: true })
      expect(parsed.entries.map((entry) => entry.seq)).toEqual([1, 3])

      // Unticked: everything shown, the search only described.
      await fireEvent.click(screen.getByRole('button', { name: /^Export/ }))
      await fireEvent.click(screen.getByRole('checkbox', { name: 'Only search matches' }))
      await fireEvent.click(screen.getByRole('button', { name: /Download \.txt/ }))
      const text = downloaded().text
      expect(text).toContain('Search (text): sale\n')
      expect(text).toContain('Entries: 3\n')
    })

    it('closes on Escape and on a press outside', async () => {
      const { connection, scroller } = setup({ source: SOURCE })
      connection.receive(msg.subscribed(1))
      connection.receive(msg.logs(records(1, 3)))
      await tick()
      const exportButton = screen.getByRole('button', { name: /^Export/ })
      await fireEvent.click(exportButton)
      await fireEvent.keyDown(screen.getByRole('button', { name: /Download \.log/ }), { key: 'Escape' })
      expect(exportButton).toHaveAttribute('aria-expanded', 'false')
      expect(exportButton).toHaveFocus()
      await fireEvent.click(exportButton)
      await fireEvent.pointerDown(scroller)
      expect(screen.queryByRole('button', { name: /Download \.log/ })).toBeNull()
    })
  })
})
