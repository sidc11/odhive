// Pure helpers of the config builder: which options the form shows, how
// they are grouped and searched, and the value checks. The checks mirror the
// server (core `conf::validate_key`, `validate_value`, `check_value`) so
// problems show while typing; the server checks again on save.
import type { OdooOption } from '../api'

/** Section title of options that only exist in `odoo.conf` (no flag, no `--help` group). */
export const FILE_ONLY_GROUP = 'Config file only'

/** Longest accepted option key (core `conf::MAX_KEY_LEN`). */
export const MAX_KEY_LEN = 128

export type Widget = 'bool' | 'choice' | 'number' | 'text' | 'secret'

export interface Section {
  title: string
  /** Stable id for anchors and `aria-controls`. */
  id: string
  options: OdooOption[]
}

/** Options the form edits: those Odoo reads from a file and OdHive does not set itself. */
export function editableOptions(options: readonly OdooOption[], managed: readonly string[]): OdooOption[] {
  return options.filter((o) => o.file_loadable && !managed.includes(o.key))
}

function sectionId(title: string): string {
  return `options-${title.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')}`
}

/** Options grouped by their `--help` section, in Odoo's order. */
export function sections(options: readonly OdooOption[]): Section[] {
  const out: Section[] = []
  for (const option of options) {
    const title = option.group ?? FILE_ONLY_GROUP
    let section = out.find((s) => s.title === title)
    if (!section) {
      section = { title, id: sectionId(title), options: [] }
      out.push(section)
    }
    section.options.push(option)
  }
  return out
}

export function widgetOf(option: OdooOption, secret: boolean): Widget {
  if (secret || option.secret) return 'secret'
  switch (option.type) {
    case 'bool':
      return 'bool'
    case 'choice':
      return 'choice'
    case 'int':
    case 'float':
      return 'number'
    default:
      return 'text'
  }
}

/** A schema default written the way Odoo writes it (core `conf::format_default`); `null` = none. */
export function formatDefault(value: unknown): string | null {
  if (value === null || value === undefined) return null
  if (typeof value === 'boolean') return value ? 'True' : 'False'
  if (typeof value === 'number') return String(value)
  if (typeof value === 'string') return value
  if (Array.isArray(value)) {
    return value
      .map(formatDefault)
      .filter((s): s is string => s !== null && s !== '')
      .join(',')
  }
  return null
}

const BOOL_WORDS = ['1', 'yes', 'true', 'on', '0', 'no', 'false', 'off']
const INT = /^[+-]?\d+$/
const FLOAT = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$|^[+-]?(inf|infinity|nan)$/i
const KEY = /^[a-z0-9_.-]+$/

/** Why `value` cannot be saved for `option` (unknown options: format only), or `null`. */
export function valueProblem(option: OdooOption | undefined, value: string): string | null {
  if (/[\n\r\0]/.test(value)) return 'Must be on one line'
  if (value !== value.trim()) return 'Remove the spaces at the start or end'
  if (!option || value === 'None') return null
  switch (option.type) {
    case 'int':
      return INT.test(value) ? null : 'Enter a whole number'
    case 'float':
      return FLOAT.test(value) ? null : 'Enter a number'
    case 'bool':
      return BOOL_WORDS.includes(value.toLowerCase()) ? null : 'Enter True or False'
    case 'choice':
      return option.choices.includes(value) ? null : `Choose one of ${option.choices.join(', ')}`
    default:
      return null
  }
}

/** Why `key` cannot be used for an option the schema does not know, or `null`. */
export function keyProblem(key: string, known: readonly string[], managed: readonly string[]): string | null {
  if (key === '') return 'Enter a key'
  if (key.length > MAX_KEY_LEN) return `At most ${MAX_KEY_LEN} characters`
  if (!KEY.test(key)) return 'Lowercase letters, digits, _ . - only'
  if (managed.includes(key)) return `${key} is set by OdHive`
  if (known.includes(key)) return `${key} has its own field above`
  return null
}

/** Whether every word of `query` occurs in the option's key, flags, help or section. */
export function matchesSearch(option: OdooOption, query: string): boolean {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean)
  if (words.length === 0) return true
  const text = [option.key, ...option.flags, option.help ?? '', option.group ?? FILE_ONLY_GROUP]
    .join('\n')
    .toLowerCase()
  return words.every((word) => text.includes(word))
}

/** Values offered by a bool or choice select besides "Default"; a saved value outside them is kept. */
export function selectValues(option: OdooOption, current: string): string[] {
  const values = option.type === 'choice' ? [...option.choices] : option.type === 'bool' ? ['True', 'False'] : []
  if (current !== '' && !values.includes(current)) values.push(current)
  return values
}
