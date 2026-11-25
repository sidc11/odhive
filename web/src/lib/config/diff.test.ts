import { describe, expect, it } from 'vitest'
import { changeCount, diffLines, MAX_CELLS } from './diff'

const show = (before: string, after: string): string[] =>
  diffLines(before, after).map((l) => `${{ same: ' ', add: '+', del: '-' }[l.kind]}${l.text}`)

describe('diffLines', () => {
  it('keeps common lines and marks removed before added', () => {
    expect(show('[options]\na = 1\nb = 2\nc = 3\n', '[options]\na = 1\nb = 5\nc = 3\nd = 4\n')).toEqual([
      ' [options]',
      ' a = 1',
      '-b = 2',
      '+b = 5',
      ' c = 3',
      '+d = 4',
    ])
  })

  it('handles empty texts and identical ones', () => {
    expect(show('', '')).toEqual([])
    expect(show('', 'a\n')).toEqual(['+a'])
    expect(show('a\nb', 'a\nb\n')).toEqual([' a', ' b'])
    expect(changeCount(diffLines('x\ny\n', 'x\ny\n'))).toBe(0)
    expect(changeCount(diffLines('x\ny\n', 'y\nz\n'))).toBe(2)
  })

  it('gives up line matching for huge inputs instead of using quadratic memory', () => {
    const n = Math.ceil(Math.sqrt(MAX_CELLS)) + 1
    const big = Array.from({ length: n }, (_, i) => `k${i} = ${i}`).join('\n')
    const diff = diffLines(big, big)
    expect(diff).toHaveLength(2 * n)
    expect(changeCount(diff)).toBe(2 * n)
  })
})
