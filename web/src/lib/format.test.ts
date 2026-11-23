import { describe, expect, it } from 'vitest'
import { formatBytes, formatCpu, formatUptime } from './format'

describe('formatBytes', () => {
  it('picks a binary unit', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(1536)).toBe('1.5 KiB')
    expect(formatBytes(250 * 1024 * 1024)).toBe('250 MiB')
    expect(formatBytes(3.25 * 1024 ** 3)).toBe('3.3 GiB')
    expect(formatBytes(-1)).toBe('0 B')
  })
})

describe('formatCpu', () => {
  it('keeps a decimal for small values', () => {
    expect(formatCpu(0)).toBe('0.0%')
    expect(formatCpu(3.14)).toBe('3.1%')
    expect(formatCpu(152.3)).toBe('152%')
  })
})

describe('formatUptime', () => {
  const at = (seconds: number): string => formatUptime(1000, (1000 + seconds) * 1000)

  it('shows the two largest units', () => {
    expect(at(0)).toBe('0s')
    expect(at(45)).toBe('45s')
    expect(at(192)).toBe('3m 12s')
    expect(at(2 * 3600 + 5 * 60 + 9)).toBe('2h 05m')
    expect(at(4 * 86_400 + 3 * 3600)).toBe('4d 3h')
  })

  it('never goes negative (clock skew)', () => {
    expect(at(-30)).toBe('0s')
  })
})
