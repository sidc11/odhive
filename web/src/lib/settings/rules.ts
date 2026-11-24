// Checks of the instance settings form. They mirror the server's rules
// (core `project::is_managed_option`, `names::validate_env_key`) so problems
// show while typing; the server checks again on save.
import type { AutofixMode, ReloadMode } from '../api'

export interface ModeChoice<T> {
  value: T
  label: string
  description: string
}

export const RELOAD_MODES: ModeChoice<ReloadMode>[] = [
  {
    value: 'auto',
    label: 'Smart',
    description:
      'Python changes restart Odoo, data files and manifests restart it with -u <module>, static files only refresh the browser.',
  },
  {
    value: 'restart',
    label: 'Restart only',
    description: 'Code and data changes restart Odoo without updating modules; static files refresh the browser.',
  },
  { value: 'off', label: 'Off', description: 'Files are not watched.' },
]

export const AUTOFIX_MODES: ModeChoice<AutofixMode>[] = [
  {
    value: 'auto',
    label: 'Automatic',
    description: 'Packages from trusted sources are installed and Odoo restarts; you are asked about any others.',
  },
  { value: 'ask', label: 'Ask first', description: 'You are asked before any package is installed.' },
  { value: 'off', label: 'Off', description: 'Missing packages are only reported.' },
]

/** Whether `arg` sets one of `managed` (`--opt`, `--opt=v`, `-o`, `-ov`). */
export function isManagedOption(arg: string, managed: readonly string[]): boolean {
  return managed.some((opt) => {
    if (opt.startsWith('--')) return arg === opt || arg.startsWith(`${opt}=`)
    return !arg.startsWith('--') && arg.startsWith(opt)
  })
}

/** One argument per non-blank line, without surrounding whitespace. */
export function parseArgs(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '')
}

export function formatArgs(args: readonly string[]): string {
  return args.join('\n')
}

export interface ArgNote {
  /** The argument as entered. */
  arg: string
  message: string
  /** `false` = a hint that does not block saving. */
  blocking: boolean
}

/** Problems and hints for the extra arguments. */
export function argNotes(args: readonly string[], managed: readonly string[]): ArgNote[] {
  const notes: ArgNote[] = []
  for (const arg of args) {
    if (arg.includes('\0')) {
      notes.push({ arg, message: 'contains a NUL character', blocking: true })
    } else if (isManagedOption(arg, managed)) {
      notes.push({ arg, message: 'is set by OdHive (ports, database, addons path, config, workers)', blocking: true })
    } else if (arg.startsWith('-') && /\s/.test(arg)) {
      // Each line is one argv entry: "--dev xml" would reach Odoo as one word.
      const [option = arg] = arg.split(/\s+/)
      notes.push({
        arg,
        message: `is passed as one argument; write ${option}=… or put the value on its own line`,
        blocking: false,
      })
    }
  }
  return notes
}

export interface EnvRow {
  /** Stable key for the list (not the variable name). */
  id: number
  key: string
  value: string
}

const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/

/** Why the row cannot be saved, or `null`. A blank row is ignored. */
export function envRowProblem(row: EnvRow, rows: readonly EnvRow[]): string | null {
  const key = row.key.trim()
  if (key === '' && row.value === '') return null
  if (key === '') return 'Enter a name'
  if (!ENV_KEY.test(key)) return 'Letters, digits and _ only, not starting with a digit'
  if (row.value.includes('\0')) return 'The value contains a NUL character'
  const first = rows.find((other) => other.key.trim() === key)
  if (first && first.id !== row.id) return `${key} is set twice`
  return null
}

/** The variables of the non-blank rows. */
export function envFromRows(rows: readonly EnvRow[]): Record<string, string> {
  const env: Record<string, string> = {}
  for (const row of rows) {
    const key = row.key.trim()
    if (key !== '') env[key] = row.value
  }
  return env
}
