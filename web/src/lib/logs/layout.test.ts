import { describe, expect, it } from 'vitest'
import { RowLayout } from './layout'

describe('RowLayout', () => {
  it('stacks rows by their line counts', () => {
    const layout = new RowLayout()
    expect(layout.total).toBe(0)
    expect(layout.range(0, 10)).toEqual({ start: 0, end: 0 })
    layout.append([1, 3, 1, 0])
    expect(layout.length).toBe(4)
    // A row is at least one line.
    expect(layout.total).toBe(6)
    expect([0, 1, 2, 3, 4].map((i) => layout.offsetOf(i))).toEqual([0, 1, 4, 5, 6])
    expect(layout.linesOf(1)).toBe(3)
  })

  it('finds the row covering a line', () => {
    const layout = new RowLayout()
    layout.append([1, 3, 1])
    expect([-5, 0, 0.5, 1, 3.9, 4, 4.5, 99].map((line) => layout.indexAt(line))).toEqual([0, 0, 0, 1, 1, 2, 2, 2])
  })

  it('gives the rows a viewport needs, with overscan', () => {
    const layout = new RowLayout()
    layout.append(Array.from({ length: 1000 }, () => 1))
    expect(layout.range(100, 20)).toEqual({ start: 100, end: 121 })
    expect(layout.range(100, 20, 10)).toEqual({ start: 90, end: 131 })
    expect(layout.range(0, 20, 10)).toEqual({ start: 0, end: 31 })
    expect(layout.range(990, 20, 10)).toEqual({ start: 980, end: 1000 })
  })

  it('drops rows from the front and shifts the rest up', () => {
    const layout = new RowLayout()
    layout.append([2, 3, 1])
    expect(layout.dropFront(2)).toBe(5)
    expect(layout.length).toBe(1)
    expect(layout.total).toBe(1)
    expect(layout.offsetOf(0)).toBe(0)
    expect(layout.dropFront(5)).toBe(1)
    expect(layout.length).toBe(0)
    expect(layout.dropFront(1)).toBe(0)
  })

  it('handles 200k rows quickly', () => {
    const layout = new RowLayout()
    const started = performance.now()
    for (let batch = 0; batch < 400; batch += 1) layout.append(Array.from({ length: 500 }, (_, i) => 1 + (i % 7 === 0 ? 4 : 0)))
    for (let i = 0; i < 10_000; i += 1) layout.range(i * 37, 40, 30)
    layout.dropFront(20_000)
    expect(layout.length).toBe(180_000)
    expect(performance.now() - started).toBeLessThan(1000)
  })
})
