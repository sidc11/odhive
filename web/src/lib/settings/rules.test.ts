import { describe, expect, it } from 'vitest'
import { argNotes, envFromRows, envRowProblem, formatArgs, isManagedOption, parseArgs, type EnvRow } from './rules'

// The server's list (core `project::MANAGED_OPTIONS`).
const MANAGED = [
  '-c',
  '--config',
  '-s',
  '--save',
  '-p',
  '--http-port',
  '--gevent-port',
  '--longpolling-port',
  '-d',
  '--database',
  '--addons-path',
  '-D',
  '--data-dir',
  '--logfile',
  '--workers',
]

describe('isManagedOption', () => {
  it('matches the server: --opt, --opt=value, -o, -ovalue', () => {
    for (const arg of ['-c', '-c/etc/odoo.conf', '--config=/x', '-p8070', '--http-port', '--http-port=1', '-dprod', '--workers=4', '-s']) {
      expect(isManagedOption(arg, MANAGED), arg).toBe(true)
    }
    for (const arg of ['--dev=xml', '--dev', '--log-level=debug', '--db-filter=^a$', '--config-file-is-not-a-thing', '-u', '-i', 'base']) {
      expect(isManagedOption(arg, MANAGED), arg).toBe(false)
    }
  })
})

describe('extra arguments', () => {
  it('are one per line, trimmed, blank lines dropped', () => {
    expect(parseArgs('  --dev=xml \n\n--log-level=debug\n  \n')).toEqual(['--dev=xml', '--log-level=debug'])
    expect(parseArgs('')).toEqual([])
    expect(formatArgs(['-u', 'sale'])).toBe('-u\nsale')
  })

  it('block managed options and NUL, hint at an option with a space', () => {
    expect(argNotes(['--dev=xml', '-u', 'sale'], MANAGED)).toEqual([])
    const notes = argNotes(['--http-port=9000', 'a\0b', '--dev xml'], MANAGED)
    expect(notes.map((n) => [n.arg, n.blocking])).toEqual([
      ['--http-port=9000', true],
      ['a\0b', true],
      ['--dev xml', false],
    ])
    expect(notes[2]?.message).toContain('--dev=…')
  })
})

describe('environment rows', () => {
  const row = (id: number, key: string, value = 'v'): EnvRow => ({ id, key, value })

  it('ignore blank rows and check names like the server', () => {
    const rows = [row(0, '', ''), row(1, 'PGHOST'), row(2, '_x1'), row(3, '1BAD'), row(4, 'BAD-KEY'), row(5, '', 'orphan')]
    expect(rows.map((r) => envRowProblem(r, rows))).toEqual([
      null,
      null,
      null,
      'Letters, digits and _ only, not starting with a digit',
      'Letters, digits and _ only, not starting with a digit',
      'Enter a name',
    ])
  })

  it('flag a name used twice (the later row) and NUL in values', () => {
    const first = row(0, 'A')
    const again = row(1, ' A ')
    const nul = row(2, 'B', 'x\0y')
    const rows = [first, again, nul]
    expect(envRowProblem(first, rows)).toBeNull()
    expect(envRowProblem(again, rows)).toBe('A is set twice')
    expect(envRowProblem(nul, rows)).toContain('NUL')
  })

  it('become a map with trimmed names, values kept as typed', () => {
    expect(envFromRows([row(0, ' A ', ' spaced '), row(1, '', ''), row(2, 'B', '')])).toEqual({ A: ' spaced ', B: '' })
  })
})
