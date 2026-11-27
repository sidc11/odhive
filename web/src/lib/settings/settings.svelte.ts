// State of the instance settings drawer: which instance it shows, and the
// form loaded from and saved to `/projects/{name}/instances/{id}/settings`.
import type { AutofixMode, InstanceSettings, InstanceSettingsInfo, ReloadMode } from '../api'
import { ApiRequestError, type ApiClient } from '../api/client'
import { argNotes, envFromRows, envRowProblem, formatArgs, parseArgs, type ArgNote, type EnvRow } from './rules'

export interface SettingsTarget {
  project: string
  instance: string
}

/** Which instance the drawer is open for, and where focus goes back to. */
export class SettingsDrawerStore {
  target = $state<SettingsTarget | null>(null)
  private returnFocus: HTMLElement | null = null

  show(project: string, instance: string): void {
    if (!this.target) {
      const active = document.activeElement
      this.returnFocus = active instanceof HTMLElement && active !== document.body ? active : null
    }
    this.target = { project, instance }
  }

  hide(): void {
    if (!this.target) return
    this.target = null
    const target = this.returnFocus
    this.returnFocus = null
    if (target?.isConnected) target.focus()
  }
}

type SettingsApi = Pick<ApiClient, 'getInstanceSettings' | 'putInstanceSettings'>

function sameSettings(a: InstanceSettings, b: InstanceSettings): boolean {
  const env = (s: InstanceSettings): string => JSON.stringify(Object.entries(s.env).sort(([x], [y]) => x.localeCompare(y)))
  return (
    a.reload_mode === b.reload_mode &&
    a.autofix_mode === b.autofix_mode &&
    (a.sql_log ?? false) === (b.sql_log ?? false) &&
    JSON.stringify(a.extra_args) === JSON.stringify(b.extra_args) &&
    env(a) === env(b)
  )
}

export class SettingsForm {
  readonly project: string
  readonly instance: string

  status = $state<'loading' | 'ready' | 'error'>('loading')
  loadError = $state('')

  reloadMode = $state<ReloadMode>('auto')
  autofixMode = $state<AutofixMode>('auto')
  sqlLog = $state(false)
  /** Extra arguments, one per line. */
  argsText = $state('')
  envRows = $state<EnvRow[]>([])

  /** Variables every instance of the project gets (names only). */
  projectEnv = $state<string[]>([])
  managedOptions = $state<string[]>([])

  saving = $state(false)
  saveError = $state<string | null>(null)
  /** A save was tried, so problems are shown on every field. */
  tried = $state(false)
  private saved = $state<InstanceSettings | null>(null)
  private justSaved = $state(false)
  private nextRow = 0
  private readonly api: SettingsApi

  constructor(api: SettingsApi, project: string, instance: string) {
    this.api = api
    this.project = project
    this.instance = instance
  }

  get current(): InstanceSettings {
    return {
      reload_mode: this.reloadMode,
      autofix_mode: this.autofixMode,
      sql_log: this.sqlLog,
      extra_args: parseArgs(this.argsText),
      env: envFromRows(this.envRows),
    }
  }

  /** Changed since loaded or last saved. */
  get dirty(): boolean {
    return this.saved !== null && !sameSettings(this.current, this.saved)
  }

  /** The last save succeeded and nothing changed since. */
  get savedNow(): boolean {
    return this.justSaved && !this.dirty
  }

  get argNotes(): ArgNote[] {
    return argNotes(parseArgs(this.argsText), this.managedOptions)
  }

  /** Problem per env row id. */
  get envProblems(): Record<number, string> {
    const out: Record<number, string> = {}
    for (const row of this.envRows) {
      const problem = envRowProblem(row, this.envRows)
      if (problem) out[row.id] = problem
    }
    return out
  }

  /** Names of instance variables that replace a project variable. */
  get overridden(): string[] {
    const own = Object.keys(envFromRows(this.envRows))
    return this.projectEnv.filter((key) => own.includes(key))
  }

  get canSave(): boolean {
    return (
      this.status === 'ready' &&
      !this.saving &&
      this.dirty &&
      !this.argNotes.some((note) => note.blocking) &&
      Object.keys(this.envProblems).length === 0
    )
  }

  /** Adds a blank row; returns it (the reactive copy, so edits through it show). */
  addEnvRow(): EnvRow {
    this.envRows.push({ id: this.nextRow++, key: '', value: '' })
    return this.envRows[this.envRows.length - 1] as EnvRow
  }

  removeEnvRow(id: number): void {
    this.envRows = this.envRows.filter((row) => row.id !== id)
  }

  private fill(info: InstanceSettingsInfo): void {
    const s = info.settings
    this.reloadMode = s.reload_mode
    this.autofixMode = s.autofix_mode
    this.sqlLog = s.sql_log ?? false
    this.argsText = formatArgs(s.extra_args)
    this.envRows = Object.entries(s.env).map(([key, value]) => ({ id: this.nextRow++, key, value }))
    this.projectEnv = info.project_env
    this.managedOptions = info.managed_options
    this.saved = s
  }

  async load(): Promise<void> {
    this.status = 'loading'
    try {
      this.fill(await this.api.getInstanceSettings(this.project, this.instance))
      this.status = 'ready'
    } catch (error) {
      this.loadError = error instanceof ApiRequestError ? error.message : String(error)
      this.status = 'error'
    }
  }

  /** Saves the form; `true` when it was saved. */
  async save(): Promise<boolean> {
    this.tried = true
    if (!this.canSave) return false
    this.saving = true
    this.saveError = null
    try {
      this.fill(await this.api.putInstanceSettings(this.project, this.instance, this.current))
      this.justSaved = true
      this.tried = false
      return true
    } catch (error) {
      this.saveError = error instanceof ApiRequestError ? error.message : String(error)
      return false
    } finally {
      this.saving = false
    }
  }
}
