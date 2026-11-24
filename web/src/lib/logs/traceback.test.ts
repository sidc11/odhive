import { describe, expect, it } from 'vitest'
import { entryLines, type LogRow } from './rows'
import { record } from './test-fixtures'
import { decorate, fileLinks, foldable, foldSummary, shownLines, tracebackText } from './traceback'

const TRACE = [
  'Failed to load registry',
  'Traceback (most recent call last):',
  '  File "/opt/odoo/odoo/service/server.py", line 1591, in preload_registries',
  '    registry = Registry.new(dbname)',
  '  File "/home/me/shop/addons/sale_x/models/order.py", line 12, in <module>',
  '    import ldap',
  "ModuleNotFoundError: No module named 'ldap'",
].join('\n')

function entry(message: string, exceptions: string[] = []): LogRow {
  const r = record(1, message, { exceptions })
  return { id: 1, kind: 'entry', record: r, lines: entryLines(r) }
}

describe('folding', () => {
  it('folds multi-line entries with an exception', () => {
    const row = entry(TRACE, ["ModuleNotFoundError: No module named 'ldap'"])
    expect(foldable(row)).toBe(true)
    expect(shownLines(row, false)).toBe(1)
    expect(shownLines(row, true)).toBe(7)
  })

  it('leaves other rows alone', () => {
    expect(foldable(entry('one line', ['ValueError: x']))).toBe(false)
    expect(foldable(entry('two\nlines'))).toBe(false)
    expect(foldable({ id: 2, kind: 'gap', skipped: 3, lines: 1 })).toBe(false)
    expect(shownLines(entry('two\nlines'), false)).toBe(2)
  })

  it('summarises the exception and hidden lines', () => {
    expect(foldSummary({ message: TRACE, exceptions: ['A: 1', "ModuleNotFoundError: No module named 'ldap'"] }, 7)).toBe(
      "ModuleNotFoundError: No module named 'ldap' · 6 more lines",
    )
    expect(foldSummary({ message: 'x\ny', exceptions: [] }, 2)).toBe('1 more line')
    // An exception that is already the first line is not repeated.
    expect(foldSummary({ message: 'KeyError: 1\nmore', exceptions: ['KeyError: 1'] }, 2)).toBe('1 more line')
  })
})

describe('tracebackText', () => {
  it('starts at the traceback line and keeps everything after it', () => {
    expect(tracebackText(TRACE)).toBe(TRACE.slice(TRACE.indexOf('Traceback')))
    const chained = `${TRACE}\n\nDuring handling of the above exception, another exception occurred:\n\nTraceback (most recent call last):\nValueError: x`
    expect(tracebackText(chained)).toBe(chained.slice(chained.indexOf('Traceback')))
  })

  it('is the whole message without a traceback line', () => {
    expect(tracebackText('KeyError: 1\nmore')).toBe('KeyError: 1\nmore')
    expect(tracebackText('Traceback (most recent call last):\nX: 1')).toBe('Traceback (most recent call last):\nX: 1')
  })
})

describe('fileLinks', () => {
  it('finds Python frames and XML parse locations with absolute paths', () => {
    const xml = 'odoo.tools.convert.ParseError: while parsing /home/me/shop/addons/x/views/v.xml:31'
    const message = `${TRACE}\n${xml}\n  File "relative/x.py", line 3\n  File "/a.py", line 0`
    const links = fileLinks(message)
    expect(links.map(({ path, line }) => `${path}:${line}`)).toEqual([
      '/opt/odoo/odoo/service/server.py:1591',
      '/home/me/shop/addons/sale_x/models/order.py:12',
      '/home/me/shop/addons/x/views/v.xml:31',
    ])
    for (const link of links) expect(message.slice(link.start, link.end)).toBe(link.path)
  })

  it('takes paths with spaces and quotes literally', () => {
    const [link] = fileLinks('  File "/my dir/it\'s.py", line 4, in f')
    expect(link?.path).toBe("/my dir/it's.py")
  })
})

describe('decorate', () => {
  const text = 'at File "/a/b.py", line 2 and more'
  const links = fileLinks(text)

  it('splits text around links', () => {
    expect(decorate(text, null, links)).toEqual([
      { link: null, parts: [{ text: 'at File "', match: false }] },
      { link: links[0], parts: [{ text: '/a/b.py', match: false }] },
      { link: null, parts: [{ text: '", line 2 and more', match: false }] },
    ])
  })

  it('keeps matches that cross link edges', () => {
    const pieces = decorate(text, /"\/a/g, links)
    expect(pieces.map((p) => p.parts)).toEqual([
      [
        { text: 'at File ', match: false },
        { text: '"', match: true },
      ],
      [
        { text: '/a', match: true },
        { text: '/b.py', match: false },
      ],
      [{ text: '", line 2 and more', match: false }],
    ])
    expect(
      pieces
        .flatMap((p) => p.parts)
        .map((p) => p.text)
        .join(''),
    ).toBe(text)
  })

  it('is plain highlighting without links', () => {
    expect(decorate('abc', /b/g, [])).toEqual([
      {
        link: null,
        parts: [
          { text: 'a', match: false },
          { text: 'b', match: true },
          { text: 'c', match: false },
        ],
      },
    ])
  })
})
