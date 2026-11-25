import { describe, expect, it } from 'vitest'
import { alwaysShown, eventOf, eventView, MAX_SHOWN_FILES, shownMessage } from './marks'
import { marked, record } from './test-fixtures'

const at = '2026-11-24T09:00:01.500Z'

describe('eventView', () => {
  it('names the changed file of a reload', () => {
    expect(
      eventView({ at, kind: 'reload', files: ['sale_x/models/order.py'], file_count: 1, update: [] }, ''),
    ).toEqual({ tag: 'RELOAD', icon: '↻', text: 'restarted: sale_x/models/order.py changed', tone: 'accent' })
  })

  it('says which modules a reload updates', () => {
    const view = eventView({ at, kind: 'reload', files: ['m1/views/x.xml'], file_count: 1, update: ['m1', 'm2'] }, '')
    expect(view.text).toBe('restarted with -u m1,m2: m1/views/x.xml changed')
  })

  it('lists a few changed files and counts the rest', () => {
    const files = ['a/1.py', 'a/2.py', 'a/3.py', 'a/4.py']
    const view = eventView({ at, kind: 'reload', files, file_count: 25, update: [] }, '')
    expect(MAX_SHOWN_FILES).toBe(3)
    expect(view.text).toBe('restarted: 25 files changed (a/1.py, a/2.py, a/3.py, +22 more)')
    expect(eventView({ at, kind: 'reload', files: ['a/1.py', 'a/2.py'], file_count: 2, update: [] }, '').text).toBe(
      'restarted: 2 files changed (a/1.py, a/2.py)',
    )
  })

  it('reports a reload after lost watch events without a count', () => {
    expect(eventView({ at, kind: 'reload', files: [], file_count: 0, update: [] }, '').text).toBe(
      'restarted: files changed',
    )
  })

  it('shows the steps of a dependency install', () => {
    expect(
      eventView({ at, kind: 'installing', package: 'Pillow', module: 'PIL', attempt: 1, max: 3 }, ''),
    ).toEqual({ tag: 'FIX', icon: '📦', text: 'installing Pillow for missing module PIL (1/3)', tone: 'accent' })
    expect(eventView({ at, kind: 'installed', package: 'Pillow' }, '')).toEqual({
      tag: 'FIX',
      icon: '📦',
      text: 'installed Pillow, restarting',
      tone: 'ok',
    })
    expect(eventView({ at, kind: 'install_failed', package: 'Pillow' }, '').tone).toBe('error')
    expect(eventView({ at, kind: 'install_cancelled', package: 'Pillow' }, '').text).toBe(
      'install of Pillow cancelled',
    )
    // Installer output is shown as it came.
    expect(eventView({ at, kind: 'install_output' }, 'odhive: Collecting Pillow')).toEqual({
      tag: 'FIX',
      icon: '',
      text: 'odhive: Collecting Pillow',
      tone: 'muted',
    })
  })

  it('shows a process start', () => {
    expect(eventView({ at, kind: 'started', pid: 77 }, '').text).toBe('started (pid 77)')
  })
})

describe('marked records', () => {
  it('only treats records the server marked as events', () => {
    // Odoo output that merely looks like OdHive's line stays output.
    const lookalike = record(1, 'odhive: installed Pillow; restarting', { level: null, logger: null })
    expect(eventOf(lookalike)).toBeNull()
    expect(alwaysShown(lookalike)).toBe(false)
    expect(shownMessage(lookalike)).toBe('odhive: installed Pillow; restarting')

    const real = marked(2, { kind: 'installed', package: 'Pillow' }, 'odhive: installed Pillow; restarting')
    expect(eventOf(real)?.text).toBe('installed Pillow, restarting')
    expect(alwaysShown(real)).toBe(true)
    expect(shownMessage(real)).toBe('installed Pillow, restarting')
  })

  it('lets installer output follow the filters', () => {
    expect(alwaysShown(marked(1, { kind: 'install_output' }))).toBe(false)
  })
})
