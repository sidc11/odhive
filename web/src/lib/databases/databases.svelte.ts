// State of a project's database page: the databases, backups and
// snapshots, the dialog open on it and the job it started.
import type { Backup, Database, DatabaseList, Job, Snapshot } from '../api'
import { ApiRequestError, type ApiClient, type UploadProgress } from '../api/client'
import type { JobsStore } from '../jobs.svelte'
import { projectDatabaseJobs } from './progress'

/** Odoo's rule for a new database name (OdHive's own is looser). */
const NEW_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{1,62}$/

/** Why `name` cannot be a new database's name, or `null`. */
export function newNameProblem(name: string): string | null {
  if (name === '') return 'Enter a name.'
  if (name.length > 63) return 'At most 63 characters.'
  if (!NEW_NAME.test(name)) return 'Letters, digits, “_”, “.” and “-”, at least 2, starting with a letter or digit.'
  return null
}

/** Module names from free text (commas, spaces or new lines between them). */
export function parseModules(text: string): string[] {
  const names = text.split(/[\s,]+/).filter((name) => name !== '')
  return names.filter((name, index) => names.indexOf(name) === index)
}

/** Why the module names cannot be sent, or `null`. */
export function modulesProblem(modules: readonly string[]): string | null {
  const bad = modules.filter((name) => !/^[a-z0-9_]+$/.test(name))
  return bad.length > 0 ? `Not a module name: ${bad.join(', ')}` : null
}

export type Dialog =
  | { kind: 'create' }
  | { kind: 'restore' }
  | { kind: 'delete'; database: Database }
  | { kind: 'duplicate'; database: Database }
  | { kind: 'backup'; database: Database }
  | { kind: 'snapshot'; database: Database }
  | { kind: 'restore_snapshot'; snapshot: Snapshot }
  | { kind: 'delete_snapshot'; snapshot: Snapshot }
  | { kind: 'delete_backup'; backup: Backup }
  /** Progress, then the outcome, of job `id`. */
  | { kind: 'job'; id: number }

type Loaded<T> = { status: 'loading' } | { status: 'ready'; value: T } | { status: 'error'; message: string }

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

export type DatabasesApi = Pick<
  ApiClient,
  | 'listDatabases'
  | 'listBackups'
  | 'listSnapshots'
  | 'createDatabase'
  | 'restoreDatabase'
  | 'deleteDatabase'
  | 'duplicateDatabase'
  | 'backupDatabase'
  | 'snapshotDatabase'
  | 'restoreSnapshot'
  | 'deleteSnapshot'
  | 'deleteBackup'
  | 'backupUrl'
  | 'cancelJob'
>

export class DatabasesStore {
  databases = $state<Loaded<DatabaseList>>({ status: 'loading' })
  backups = $state<Loaded<Backup[]>>({ status: 'loading' })
  snapshots = $state<Loaded<Snapshot[]>>({ status: 'loading' })

  dialog = $state<Dialog | null>(null)
  /** A request of the open dialog is on its way. */
  busy = $state(false)
  /** Why the open dialog's last request was refused. */
  error = $state<ApiRequestError | null>(null)
  /** Of the restore upload, while it goes. */
  upload = $state<UploadProgress | null>(null)

  private uploadAbort: AbortController | null = null
  /** Finished jobs the lists were already reloaded for. */
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  private reloadedFor = new Set<number>()
  private loads = 0

  readonly api: DatabasesApi
  readonly jobs: Pick<JobsStore, 'jobs' | 'get' | 'upsert' | 'finished'>
  readonly project: string

  constructor(api: DatabasesApi, jobs: Pick<JobsStore, 'jobs' | 'get' | 'upsert' | 'finished'>, project: string) {
    this.api = api
    this.jobs = jobs
    this.project = project
    for (const job of projectDatabaseJobs(jobs.jobs, project)) {
      if (job.state !== 'running') this.reloadedFor.add(job.id)
    }
  }

  /** Database jobs of the project still running (also those started elsewhere). */
  get running(): Job[] {
    return projectDatabaseJobs(this.jobs.jobs, this.project).filter((job) => job.state === 'running')
  }

  /** The running job working on `database`, if any. */
  jobOn(database: string): Job | undefined {
    return this.running.find(
      (job) => 'database' in job && (job.database === database || ('source' in job && job.source === database)),
    )
  }

  /** The listed database named `name`. */
  find(name: string): Database | undefined {
    return this.databases.status === 'ready' ? this.databases.value.databases.find((db) => db.name === name) : undefined
  }

  async load(): Promise<void> {
    const load = ++this.loads
    const settle = async <T>(promise: Promise<T>): Promise<Loaded<T>> => {
      try {
        return { status: 'ready', value: await promise }
      } catch (error) {
        return { status: 'error', message: message(error) }
      }
    }
    const [databases, backups, snapshots] = await Promise.all([
      settle(this.api.listDatabases(this.project)),
      settle(this.api.listBackups(this.project)),
      settle(this.api.listSnapshots(this.project)),
    ])
    // An older load that answers late must not win.
    if (load !== this.loads) return
    this.databases = databases
    this.backups = backups
    this.snapshots = snapshots
  }

  /** Reloads once for each database job of the project that ended. */
  sync(): void {
    let ended = false
    for (const job of projectDatabaseJobs(this.jobs.jobs, this.project)) {
      if (job.state === 'running' || this.reloadedFor.has(job.id)) continue
      this.reloadedFor.add(job.id)
      ended = true
    }
    if (ended) void this.load()
  }

  open(dialog: Dialog): void {
    this.dialog = dialog
    this.error = null
    this.busy = false
    this.upload = null
  }

  /** Closes the dialog; an upload on its way is stopped. A started job goes on. */
  close(): void {
    this.uploadAbort?.abort()
    this.uploadAbort = null
    this.dialog = null
    this.error = null
    this.busy = false
    this.upload = null
  }

  /** Sends the dialog's request; a started job takes over the dialog. */
  private async start(request: () => Promise<Job>): Promise<boolean> {
    if (this.busy) return false
    const dialog = this.dialog
    this.busy = true
    this.error = null
    try {
      const job = await request()
      this.jobs.upsert(job)
      if (this.dialog === dialog) this.dialog = { kind: 'job', id: job.id }
      return true
    } catch (error) {
      if (this.dialog === dialog) {
        this.error = error instanceof ApiRequestError ? error : new ApiRequestError(0, 'error', message(error))
      }
      return false
    } finally {
      if (this.dialog === dialog || this.dialog?.kind === 'job') this.busy = false
      this.upload = null
    }
  }

  /** Runs a quick request (no job): closes the dialog and reloads on success. */
  private async quick(request: () => Promise<void>): Promise<boolean> {
    if (this.busy) return false
    const dialog = this.dialog
    this.busy = true
    this.error = null
    try {
      await request()
      if (this.dialog === dialog) this.close()
      void this.load()
      return true
    } catch (error) {
      if (this.dialog === dialog) {
        this.error = error instanceof ApiRequestError ? error : new ApiRequestError(0, 'error', message(error))
      }
      return false
    } finally {
      if (this.dialog === dialog) this.busy = false
    }
  }

  create(body: Parameters<DatabasesApi['createDatabase']>[1]): Promise<boolean> {
    return this.start(() => this.api.createDatabase(this.project, body))
  }

  restore(file: Blob, query: { name: string; neutralize: boolean; copy: boolean }): Promise<boolean> {
    const abort = new AbortController()
    this.uploadAbort = abort
    this.upload = { sent: 0, total: file.size }
    return this.start(() =>
      this.api.restoreDatabase(this.project, file, query, {
        signal: abort.signal,
        onprogress: (progress) => {
          if (this.uploadAbort === abort) this.upload = progress
        },
      }),
    ).finally(() => {
      if (this.uploadAbort === abort) this.uploadAbort = null
    })
  }

  delete(database: string, confirm: string, snapshot: boolean): Promise<boolean> {
    return this.start(() => this.api.deleteDatabase(this.project, database, { confirm, snapshot }))
  }

  duplicate(database: string, body: { name: string; neutralize: boolean; stop_instances: boolean }): Promise<boolean> {
    return this.start(() => this.api.duplicateDatabase(this.project, database, body))
  }

  backup(database: string, format: 'zip' | 'dump'): Promise<boolean> {
    return this.start(() => this.api.backupDatabase(this.project, database, { format }))
  }

  snapshot(database: string, stopInstances: boolean): Promise<boolean> {
    return this.start(() => this.api.snapshotDatabase(this.project, database, { stop_instances: stopInstances }))
  }

  restoreSnapshot(snapshot: string, stopInstances: boolean): Promise<boolean> {
    return this.start(() => this.api.restoreSnapshot(this.project, snapshot, { stop_instances: stopInstances }))
  }

  deleteSnapshot(snapshot: string): Promise<boolean> {
    return this.quick(() => this.api.deleteSnapshot(this.project, snapshot))
  }

  deleteBackup(file: string): Promise<boolean> {
    return this.quick(() => this.api.deleteBackup(this.project, file))
  }

  /** Asks the server to cancel job `id` (the job reports the outcome). */
  async cancel(id: number): Promise<void> {
    try {
      this.jobs.upsert(await this.api.cancelJob(id))
    } catch (error) {
      this.error = error instanceof ApiRequestError ? error : new ApiRequestError(0, 'error', message(error))
    }
  }

  backupUrl(file: string): string {
    return this.api.backupUrl(this.project, file)
  }
}
