// State of the config builder: the project's option schema and saved
// options, the values being edited, and saving them through
// `PUT /projects/{name}/options`.
import type { ImportedOptions, OdooOption, OptionsUpdate, ProjectOptions, SchemaInfo } from '../api'
import { ApiRequestError, type ApiClient } from '../api/client'
import { editableOptions, keyProblem, matchesSearch, sections, valueProblem, type Section } from './fields'

type ConfigApi = Pick<ApiClient, 'projectSchema' | 'getProjectOptions' | 'putProjectOptions' | 'importProjectOptions'>

/** Largest `odoo.conf` the import reads (the API takes bodies up to 1 MiB). */
export const MAX_IMPORT_BYTES = 512 * 1024

/** An option the schema does not know (read by a third-party module). */
export interface CustomRow {
  /** Stable key for the list (not the option key). */
  id: number
  key: string
  value: string
}

/** What happens to a secret on save: kept, replaced by a new value, or removed. */
export type SecretEdit = { action: 'keep' } | { action: 'set'; value: string } | { action: 'remove' }

function message(error: unknown): string {
  return error instanceof ApiRequestError ? error.message : String(error)
}

function sameRecord(a: Record<string, string>, b: Record<string, string>): boolean {
  const keys = Object.keys(a)
  return keys.length === Object.keys(b).length && keys.every((k) => b[k] === a[k])
}

export class ConfigForm {
  readonly project: string

  status = $state<'loading' | 'ready' | 'error'>('loading')
  loadError = $state('')
  /** Where the option list came from (`bundled` until the project has a venv). */
  schemaSource = $state<SchemaInfo['source'] | null>(null)
  schemaReason = $state<string | null>(null)
  series = $state('')

  /** Options the form edits, in Odoo's order. */
  options = $state.raw<OdooOption[]>([])
  managedKeys = $state.raw<string[]>([])
  secretKeys = $state.raw<string[]>([])
  /** Secrets that have a saved value. */
  secretsSet = $state.raw<string[]>([])
  /** The `odoo.conf` OdHive writes for the project. */
  confPath = $state('')
  importing = $state(false)
  /** What the last import did not take over (or why it failed). */
  importNotes = $state.raw<string[]>([])
  importError = $state<string | null>(null)

  /** Value per schema option key; '' = not set (Odoo's default). */
  values = $state<Record<string, string>>({})
  customRows = $state<CustomRow[]>([])
  secrets = $state<Record<string, SecretEdit>>({})

  query = $state('')
  onlySet = $state(false)

  saving = $state(false)
  saveError = $state<string | null>(null)
  /** A save was tried, so problems are shown on every field. */
  tried = $state(false)
  private saved = $state.raw<Record<string, string>>({})
  private justSaved = $state(false)
  private nextRow = 0
  private readonly api: ConfigApi

  constructor(api: ConfigApi, project: string) {
    this.api = api
    this.project = project
  }

  isSecret(option: OdooOption): boolean {
    return option.secret || this.secretKeys.includes(option.key)
  }

  /** Non-secret options as they would be saved. */
  get current(): Record<string, string> {
    const out: Record<string, string> = {}
    for (const [key, value] of Object.entries(this.values)) if (value !== '') out[key] = value
    for (const row of this.customRows) {
      const key = row.key.trim()
      if (key !== '') out[key] = row.value
    }
    return out
  }

  get secretChanges(): NonNullable<OptionsUpdate['secrets']> {
    const out: NonNullable<OptionsUpdate['secrets']> = {}
    for (const [key, edit] of Object.entries(this.secrets)) {
      if (edit.action === 'set' && edit.value !== '') out[key] = edit.value
      else if (edit.action === 'remove' && this.secretsSet.includes(key)) out[key] = null
    }
    return out
  }

  /** The request body of a save (and of a preview). */
  get update(): OptionsUpdate {
    return { options: this.current, secrets: this.secretChanges }
  }

  get dirty(): boolean {
    return (
      this.status === 'ready' && (!sameRecord(this.current, this.saved) || Object.keys(this.secretChanges).length > 0)
    )
  }

  /** The last save succeeded and nothing changed since. */
  get savedNow(): boolean {
    return this.justSaved && !this.dirty
  }

  /** Problem per schema option key. */
  get problems(): Record<string, string> {
    const out: Record<string, string> = {}
    for (const option of this.options) {
      const value = this.values[option.key] ?? ''
      const problem = value === '' ? null : valueProblem(option, value)
      if (problem) out[option.key] = problem
    }
    for (const [key, edit] of Object.entries(this.secrets)) {
      if (edit.action !== 'set' || edit.value === '') continue
      const problem = valueProblem(
        this.options.find((o) => o.key === key),
        edit.value,
      )
      if (problem) out[key] = problem
    }
    return out
  }

  /** Problem per custom row id. A blank row is ignored. */
  get rowProblems(): Record<number, string> {
    const out: Record<number, string> = {}
    const known = this.options.map((o) => o.key).concat(this.secretKeys)
    for (const row of this.customRows) {
      const key = row.key.trim()
      if (key === '' && row.value === '') continue
      const first = this.customRows.find((other) => other.key.trim() === key)
      const problem =
        keyProblem(key, known, this.managedKeys) ??
        (first && first.id !== row.id ? `${key} is set twice` : null) ??
        valueProblem(undefined, row.value)
      if (problem) out[row.id] = problem
    }
    return out
  }

  get problemCount(): number {
    return Object.keys(this.problems).length + Object.keys(this.rowProblems).length
  }

  get canSave(): boolean {
    return this.status === 'ready' && !this.saving && this.dirty && this.problemCount === 0
  }

  /** Whether the option has a value (saved secret, or a non-default value). */
  isSet(option: OdooOption): boolean {
    if (this.isSecret(option)) {
      const edit = this.secrets[option.key]
      if (edit?.action === 'set') return edit.value !== ''
      if (edit?.action === 'remove') return false
      return this.secretsSet.includes(option.key)
    }
    return (this.values[option.key] ?? '') !== ''
  }

  /** Sections with the options matching the search (and "only set"), empty ones left out. */
  get shownSections(): Section[] {
    const shown = this.options.filter(
      (option) => matchesSearch(option, this.query) && (!this.onlySet || this.isSet(option)),
    )
    return sections(shown)
  }

  get shownCount(): number {
    return this.shownSections.reduce((n, s) => n + s.options.length, 0)
  }

  /** Changes the value of a schema option ('' = back to the default). */
  setValue(key: string, value: string): void {
    this.values[key] = value
    this.justSaved = false
  }

  setSecret(key: string, edit: SecretEdit): void {
    this.secrets[key] = edit
    this.justSaved = false
  }

  /** Adds a blank custom option; returns it (the reactive copy, so edits through it show). */
  addRow(): CustomRow {
    this.customRows.push({ id: this.nextRow++, key: '', value: '' })
    return this.customRows[this.customRows.length - 1] as CustomRow
  }

  removeRow(id: number): void {
    this.customRows = this.customRows.filter((row) => row.id !== id)
  }

  /** Back to the saved options. */
  discard(): void {
    this.fill(
      {
        options: this.saved,
        secrets_set: this.secretsSet,
        secret_keys: this.secretKeys,
        managed_keys: this.managedKeys,
        conf_path: this.confPath,
      },
      false,
    )
    this.tried = false
    this.saveError = null
  }

  private fill(saved: ProjectOptions, keepRows: boolean): void {
    this.managedKeys = saved.managed_keys
    this.secretKeys = saved.secret_keys
    this.secretsSet = saved.secrets_set
    this.confPath = saved.conf_path
    this.saved = saved.options
    const known = this.options.map((o) => o.key)
    const values: Record<string, string> = {}
    const custom: [string, string][] = []
    for (const [key, value] of Object.entries(saved.options)) {
      if (known.includes(key)) values[key] = value
      else custom.push([key, value])
    }
    this.values = values
    this.secrets = {}
    // After a save the rows stay as typed (same keys, minus blank ones), so focus is not lost.
    if (!keepRows) this.customRows = custom.map(([key, value]) => ({ id: this.nextRow++, key, value }))
    else this.customRows = this.customRows.filter((row) => row.key.trim() !== '')
  }

  /**
   * Replaces the form's options with those of an `odoo.conf` (not saved
   * yet). Secrets in the file replace saved ones; others are kept.
   */
  applyImport(imported: ImportedOptions): void {
    const known = this.options.map((o) => o.key)
    const values: Record<string, string> = {}
    const custom: CustomRow[] = []
    for (const [key, value] of Object.entries(imported.options)) {
      if (known.includes(key)) values[key] = value
      else custom.push({ id: this.nextRow++, key, value })
    }
    this.values = values
    this.customRows = custom
    const secrets: Record<string, SecretEdit> = {}
    for (const [key, value] of Object.entries(imported.secrets)) secrets[key] = { action: 'set', value }
    this.secrets = secrets
    this.importNotes = imported.warnings
    this.justSaved = false
  }

  /** Reads `file` as an `odoo.conf` into the form; `true` when it was read. */
  async importFile(file: Pick<File, 'size' | 'text'>): Promise<boolean> {
    this.importError = null
    this.importNotes = []
    if (file.size > MAX_IMPORT_BYTES) {
      this.importError = `The file is larger than ${MAX_IMPORT_BYTES / 1024} KiB; an odoo.conf is much smaller.`
      return false
    }
    this.importing = true
    try {
      const text = await file.text()
      this.applyImport(await this.api.importProjectOptions(this.project, { text }))
      return true
    } catch (error) {
      this.importError = message(error)
      return false
    } finally {
      this.importing = false
    }
  }

  async load(): Promise<void> {
    this.status = 'loading'
    try {
      const [info, saved] = await Promise.all([
        this.api.projectSchema(this.project),
        this.api.getProjectOptions(this.project),
      ])
      this.schemaSource = info.source
      this.schemaReason = info.reason ?? null
      this.series = info.schema.series
      this.options = editableOptions(info.schema.options, saved.managed_keys)
      this.fill(saved, false)
      this.status = 'ready'
    } catch (error) {
      this.loadError = message(error)
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
      const saved = await this.api.putProjectOptions(this.project, this.update)
      this.fill(saved, true)
      this.justSaved = true
      this.tried = false
      return true
    } catch (error) {
      this.saveError = message(error)
      return false
    } finally {
      this.saving = false
    }
  }
}
