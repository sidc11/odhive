// Turns a database job (create, restore, delete, copy, backup, snapshot)
// into a short checklist and a one-line outcome for the UI.
import type { Job } from '../api'
import { formatBytes } from '../format'

export type DatabaseJobKind =
  | 'create_database'
  | 'restore_database'
  | 'delete_database'
  | 'duplicate_database'
  | 'backup_database'
  | 'snapshot_database'
  | 'restore_snapshot'

export const DATABASE_JOB_KINDS: readonly DatabaseJobKind[] = [
  'create_database',
  'restore_database',
  'delete_database',
  'duplicate_database',
  'backup_database',
  'snapshot_database',
  'restore_snapshot',
]

/** A job of one of the database kinds. */
export type DatabaseJob = Job & { kind: DatabaseJobKind; project: string; database: string }

export function isDatabaseJob(job: Job): job is DatabaseJob {
  return (DATABASE_JOB_KINDS as readonly string[]).includes(job.kind)
}

/** Database jobs of `project`, oldest first. */
export function projectDatabaseJobs(jobs: readonly Job[], project: string): DatabaseJob[] {
  return jobs.filter((job): job is DatabaseJob => isDatabaseJob(job) && job.project === project)
}

export type StageState = 'active' | 'done' | 'failed'

export interface Stage {
  /** The step name, e.g. `load`. */
  id: string
  label: string
  state: StageState
  /** One line about what happened, e.g. the module being installed. */
  detail?: string
}

const LABELS: Record<DatabaseJobKind, Record<string, string>> = {
  create_database: {
    initialize: 'Create the database',
    install_modules: 'Install modules',
    finalize: 'Check it and set the admin password',
    cleanup: 'Remove the unfinished database',
  },
  restore_database: {
    inspect: 'Check the file',
    create: 'Create the database',
    load: 'Load the dump',
    check: 'Check the Odoo version',
    neutralize: 'Neutralize',
    filestore: 'Put the filestore in place',
    finalize: 'Finish',
    cleanup: 'Remove the unfinished database',
  },
  delete_database: {
    snapshot: 'Take a snapshot',
    drop: 'Drop the database and its filestore',
    check: 'Check it is gone',
  },
  duplicate_database: {
    stop_instances: 'Stop the instances using it',
    copy: 'Copy the database and its filestore',
    check: 'Check the copy',
    cleanup: 'Remove the unfinished copy',
    restart_instances: 'Start the instances again',
  },
  backup_database: {
    dump: 'Dump the database',
    check: 'Check the file',
  },
  snapshot_database: {
    stop_instances: 'Stop the instances using it',
    copy: 'Copy the database',
    filestore: 'Keep the filestore',
    cleanup: 'Remove the unfinished snapshot',
    restart_instances: 'Start the instances again',
  },
  restore_snapshot: {
    stop_instances: 'Stop the instances using it',
    copy: 'Copy the snapshot',
    filestore: 'Prepare the filestore',
    swap: 'Swap in the restored database',
    cleanup: 'Remove the unfinished copy',
    restart_instances: 'Start the instances again',
  },
}

export const TITLES: Record<DatabaseJobKind, string> = {
  create_database: 'Create database',
  restore_database: 'Restore database',
  delete_database: 'Delete database',
  duplicate_database: 'Duplicate database',
  backup_database: 'Back up database',
  snapshot_database: 'Take snapshot',
  restore_snapshot: 'Restore snapshot',
}

function label(kind: DatabaseJobKind, step: string): string {
  return LABELS[kind][step] ?? step.replaceAll('_', ' ')
}

/**
 * The steps a job has reached, in order: the last one is active while the
 * job runs; on failure or cancel it is the one that failed. Steps come and
 * go with the request (modules, snapshot first, …), so only the ones seen
 * are listed.
 */
export function databaseStages(job: DatabaseJob): Stage[] {
  const stages: Stage[] = []
  const last = (): Stage | undefined => stages[stages.length - 1]
  for (const step of job.steps) {
    if (step.job !== job.kind) continue
    if (step.event === 'step') {
      const previous = last()
      if (previous) previous.state = 'done'
      stages.push({ id: step.step, label: label(job.kind, step.step), state: 'active' })
    } else if (step.event === 'module') {
      const current = last()
      if (current) current.detail = `${step.name} (${step.index}/${step.total})`
    } else if (step.event === 'inspected') {
      const current = last()
      if (current) {
        current.detail =
          step.format === 'zip'
            ? `Odoo backup, ${formatBytes(step.unpacked_bytes)} unpacked, ${step.filestore_files} filestore files`
            : FORMAT_LABELS[step.format]
      }
    }
  }
  const current = last()
  if (current && job.state === 'succeeded') current.state = 'done'
  if (current && (job.state === 'failed' || job.state === 'cancelled')) current.state = 'failed'
  return stages
}

const FORMAT_LABELS: Record<'zip' | 'custom' | 'tar' | 'sql', string> = {
  zip: 'Odoo backup (zip)',
  custom: 'pg_dump archive (custom format)',
  tar: 'pg_dump archive (tar format)',
  sql: 'SQL script',
}

/** One line about how the job ended (or that it still runs). */
export function databaseOutcome(job: DatabaseJob): string {
  if (job.state === 'running') return 'Working…'
  if (job.state === 'cancelled') return 'Cancelled. Nothing was left behind.'
  if (job.state === 'failed') return job.error?.message ?? 'Failed.'
  const result = job.result
  switch (result?.kind) {
    case 'create_database': {
      const db = result.database
      const extra = db.modules.length > 0 ? ` with ${db.modules.join(', ')}` : ''
      return `Created ${db.name}${extra}. Sign in as “${db.login}”.`
    }
    case 'restore_database': {
      const db = result.database
      const notes = [db.neutralized ? 'neutralized' : 'not neutralized', db.new_uuid ? 'new UUID' : 'UUID kept']
      if (db.filestore_files > 0) notes.push(`${db.filestore_files} filestore files`)
      return `Restored ${db.name} (${notes.join(', ')}).`
    }
    case 'delete_database': {
      const snap = result.snapshot
      return `Deleted ${result.database.name}.${snap ? ` Snapshot ${snap.name} can bring it back.` : ''}`
    }
    case 'duplicate_database': {
      const db = result.database
      return `Copied ${db.source} to ${db.name}${db.neutralized ? ' (neutralized)' : ''}.`
    }
    case 'backup_database':
      return `Saved ${result.backup.file} (${formatBytes(result.backup.size)}).`
    case 'snapshot_database':
      return `Took snapshot ${result.snapshot.name}.`
    case 'restore_snapshot':
      return `Restored ${result.database.database} from ${result.database.snapshot}.`
    default:
      return 'Done.'
  }
}
