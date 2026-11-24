import { describe, expect, it } from 'vitest'
import { gridClass, keyedPanes, paneClass, withoutPane, withPane } from './split'

const main = { project: 'shop', instance: 'main' }
const test = { project: 'shop', instance: 'test' }
const crm = { project: 'crm', instance: 'main' }

describe('split view', () => {
  it('keys panes by instance and occurrence', () => {
    expect(keyedPanes([main, test, main]).map((pane) => pane.key)).toEqual(['shop/main#0', 'shop/test#0', 'shop/main#1'])
    // Removing another pane keeps a pane's key (and so its filters).
    expect(keyedPanes([test, main]).map((pane) => pane.key)).toEqual(['shop/test#0', 'shop/main#0'])
  })

  it('adds panes up to four', () => {
    expect(withPane([], main)).toBe('/logs/shop/main')
    expect(withPane([main], crm)).toBe('/logs/shop/main/crm/main')
    expect(withPane([main, test, crm, main], test)).toBe('/logs/shop/main/shop/test/crm/main/shop/main')
  })

  it('removes a pane by position', () => {
    expect(withoutPane([main, test, main], 0)).toBe('/logs/shop/test/shop/main')
    expect(withoutPane([main], 0)).toBe('/logs')
  })

  it('lays out 1-4 panes', () => {
    expect(gridClass(2)).toContain('lg:grid-cols-2')
    expect(gridClass(4)).toContain('lg:grid-rows-[repeat(2,minmax(20rem,1fr))]')
    expect(gridClass(3)).toContain('2xl:grid-cols-3')
    expect(gridClass(9)).toBe(gridClass(4))
    expect(paneClass(3, 2)).toContain('lg:col-span-2')
    expect(paneClass(3, 1)).toBe('')
    expect(paneClass(4, 2)).toBe('')
  })
})
