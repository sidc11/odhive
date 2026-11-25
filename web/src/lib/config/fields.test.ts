import { describe, expect, it } from 'vitest'
import type { OdooOption } from '../api'
import { odooOption as option } from '../../test-fixtures'
import {
  editableOptions,
  FILE_ONLY_GROUP,
  formatDefault,
  keyProblem,
  matchesSearch,
  sections,
  selectValues,
  valueProblem,
  widgetOf,
} from './fields'

describe('editableOptions', () => {
  it('leaves out command-line only and managed options', () => {
    const all = [option('log_level'), option('save', { file_loadable: false }), option('http_port')]
    expect(editableOptions(all, ['http_port']).map((o) => o.key)).toEqual(['log_level'])
  })
})

describe('sections', () => {
  it('groups by --help section in order; file-only options get their own title', () => {
    const all = [
      option('admin_passwd', { group: null, flags: [] }),
      option('log_level', { group: 'Logging Configuration' }),
      option('db_host', { group: 'Database related options' }),
      option('log_db', { group: 'Logging Configuration' }),
    ]
    expect(sections(all).map((s) => [s.title, s.id, s.options.map((o) => o.key)])).toEqual([
      [FILE_ONLY_GROUP, 'options-config-file-only', ['admin_passwd']],
      ['Logging Configuration', 'options-logging-configuration', ['log_level', 'log_db']],
      ['Database related options', 'options-database-related-options', ['db_host']],
    ])
  })
})

describe('widgetOf', () => {
  it('picks a widget per type; secrets always mask', () => {
    expect(widgetOf(option('a', { type: 'bool' }), false)).toBe('bool')
    expect(widgetOf(option('a', { type: 'choice', choices: ['x'] } as Partial<OdooOption>), false)).toBe('choice')
    expect(widgetOf(option('a', { type: 'int' }), false)).toBe('number')
    expect(widgetOf(option('a', { type: 'float' }), false)).toBe('number')
    expect(widgetOf(option('a', { type: 'path' }), false)).toBe('text')
    expect(widgetOf(option('a', { type: 'list' }), false)).toBe('text')
    expect(widgetOf(option('a'), true)).toBe('secret')
    expect(widgetOf(option('a', { secret: true }), false)).toBe('secret')
  })
})

describe('formatDefault', () => {
  it('writes defaults the way Odoo does', () => {
    expect(formatDefault(null)).toBeNull()
    expect(formatDefault(undefined)).toBeNull()
    expect(formatDefault(true)).toBe('True')
    expect(formatDefault(false)).toBe('False')
    expect(formatDefault(8069)).toBe('8069')
    expect(formatDefault('')).toBe('')
    expect(formatDefault(['base', '', 'web'])).toBe('base,web')
    expect(formatDefault({ a: 1 })).toBeNull()
  })
})

describe('valueProblem', () => {
  const int = option('limit_time_cpu', { type: 'int' })
  const float = option('osv_memory_age_limit', { type: 'float' })
  const bool = option('list_db', { type: 'bool' })
  const choice = option('log_level', { type: 'choice', choices: ['info', 'debug'] } as Partial<OdooOption>)

  it('checks the value the way Odoo parses it', () => {
    for (const ok of ['60', '-1', '+5']) expect(valueProblem(int, ok)).toBeNull()
    for (const bad of ['1.5', 'soon', '']) expect(valueProblem(int, bad)).toBe('Enter a whole number')
    for (const ok of ['1', '1.5', '.5', '1.', '1e3', 'inf', 'NaN']) expect(valueProblem(float, ok)).toBeNull()
    expect(valueProblem(float, 'x')).toBe('Enter a number')
    for (const ok of ['True', 'false', 'YES', 'off', '1']) expect(valueProblem(bool, ok)).toBeNull()
    expect(valueProblem(bool, 'maybe')).toBe('Enter True or False')
    expect(valueProblem(choice, 'debug')).toBeNull()
    expect(valueProblem(choice, 'loud')).toBe('Choose one of info, debug')
  })

  it('accepts None (unset) for every type and anything on one line for unknown options', () => {
    for (const o of [int, float, bool, choice]) expect(valueProblem(o, 'None')).toBeNull()
    expect(valueProblem(undefined, 'whatever, really')).toBeNull()
  })

  it('refuses line breaks and surrounding spaces (configparser would change them)', () => {
    for (const o of [undefined, option('x')]) {
      expect(valueProblem(o, 'a\nb')).toBe('Must be on one line')
      expect(valueProblem(o, 'a\0')).toBe('Must be on one line')
      expect(valueProblem(o, ' a')).toBe('Remove the spaces at the start or end')
    }
  })
})

describe('keyProblem', () => {
  it('checks keys of options the schema does not know', () => {
    expect(keyProblem('x_mod.key-1', ['log_level'], ['http_port'])).toBeNull()
    expect(keyProblem('', [], [])).toBe('Enter a key')
    expect(keyProblem('Bad Key', [], [])).toBe('Lowercase letters, digits, _ . - only')
    expect(keyProblem('a'.repeat(129), [], [])).toBe('At most 128 characters')
    expect(keyProblem('http_port', [], ['http_port'])).toBe('http_port is set by OdHive')
    expect(keyProblem('log_level', ['log_level'], [])).toBe('log_level has its own field above')
  })
})

describe('matchesSearch', () => {
  const o = option('db_maxconn', { flags: ['--db_maxconn'], help: 'Maximum number of physical connections', group: 'Database related options' })

  it('needs every word in key, flags, help or section, ignoring case', () => {
    expect(matchesSearch(o, '')).toBe(true)
    expect(matchesSearch(o, 'MAXCONN')).toBe(true)
    expect(matchesSearch(o, 'physical database')).toBe(true)
    expect(matchesSearch(o, 'physical http')).toBe(false)
    expect(matchesSearch(option('admin_passwd', { group: null }), 'config file')).toBe(true)
  })
})

describe('selectValues', () => {
  it('offers True/False or the choices, keeping a saved value outside them', () => {
    expect(selectValues(option('a', { type: 'bool' }), '')).toEqual(['True', 'False'])
    expect(selectValues(option('a', { type: 'bool' }), 'yes')).toEqual(['True', 'False', 'yes'])
    const choice = option('a', { type: 'choice', choices: ['info', 'debug'] } as Partial<OdooOption>)
    expect(selectValues(choice, 'debug')).toEqual(['info', 'debug'])
    expect(selectValues(choice, 'None')).toEqual(['info', 'debug', 'None'])
  })
})
