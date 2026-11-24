import { describe, expect, it } from 'vitest'
import { entryLines, formatTime } from './rows'

describe('rows', () => {
  it('counts the lines of a message', () => {
    expect(entryLines({ message: '' })).toBe(1)
    expect(entryLines({ message: 'one' })).toBe(1)
    expect(entryLines({ message: 'Traceback:\n  File "a.py", line 1\nKeyError' })).toBe(3)
    expect(entryLines({ message: 'a\n' })).toBe(2)
  })

  it('formats timestamps as local time with milliseconds', () => {
    const local = new Date(2026, 10, 24, 9, 5, 7, 42)
    expect(formatTime(local.toISOString())).toBe('09:05:07.042')
    expect(formatTime(null)).toBe('')
    expect(formatTime('nonsense')).toBe('')
  })
})
