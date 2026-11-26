import { describe, expect, it } from 'vitest'
import { backup, databaseJob, snapshot } from '../../test-fixtures'
import type { Job, JobStep } from '../api'
import { databaseOutcome, databaseStages, isDatabaseJob, projectDatabaseJobs } from './progress'

const createSteps: JobStep[] = [
  { job: 'create_database', event: 'step', step: 'initialize' },
  { job: 'create_database', event: 'step', step: 'install_modules' },
  { job: 'create_database', event: 'module', name: 'base', index: 1, total: 3 },
  { job: 'create_database', event: 'module', name: 'sale', index: 3, total: 3 },
]

describe('databaseStages', () => {
  it('lists the steps reached; the last one runs, module progress as detail', () => {
    const job = databaseJob(1, 'create_database', 'shop_main', { steps: createSteps })
    expect(databaseStages(job)).toEqual([
      { id: 'initialize', label: 'Create the database', state: 'done' },
      { id: 'install_modules', label: 'Install modules', state: 'active', detail: 'sale (3/3)' },
    ])
  })

  it('marks the last step done on success and failed on failure or cancel', () => {
    const done = databaseJob(1, 'create_database', 'shop_main', { steps: createSteps, state: 'succeeded' })
    expect(databaseStages(done).map((s) => s.state)).toEqual(['done', 'done'])
    for (const state of ['failed', 'cancelled'] as const) {
      const job = databaseJob(1, 'create_database', 'shop_main', { steps: createSteps, state })
      expect(databaseStages(job).map((s) => s.state)).toEqual(['done', 'failed'])
    }
  })

  it('describes what a restore found in the file', () => {
    const job = databaseJob(2, 'restore_database', 'shop_copy', {
      steps: [
        { job: 'restore_database', event: 'step', step: 'inspect' },
        { job: 'restore_database', event: 'inspected', format: 'zip', unpacked_bytes: 3 * 1024 * 1024, filestore_files: 20 },
        { job: 'restore_database', event: 'step', step: 'load' },
      ],
    })
    expect(databaseStages(job)).toEqual([
      { id: 'inspect', label: 'Check the file', state: 'done', detail: 'Odoo backup, 3.0 MiB unpacked, 20 filestore files' },
      { id: 'load', label: 'Load the dump', state: 'active' },
    ])
    const sql = databaseJob(2, 'restore_database', 'shop_copy', {
      steps: [
        { job: 'restore_database', event: 'step', step: 'inspect' },
        { job: 'restore_database', event: 'inspected', format: 'sql', unpacked_bytes: 0, filestore_files: 0 },
      ],
    })
    expect(databaseStages(sql)[0]?.detail).toBe('SQL script')
  })

  it('labels snapshot steps by job kind and ignores steps of other kinds', () => {
    const job = databaseJob(3, 'restore_snapshot', 'shop_main', {
      steps: [
        { job: 'restore_snapshot', event: 'step', step: 'swap' },
        { job: 'snapshot_database', event: 'step', step: 'copy' },
      ],
    })
    expect(databaseStages(job)).toEqual([{ id: 'swap', label: 'Swap in the restored database', state: 'active' }])
  })
})

describe('databaseOutcome', () => {
  it('says how each kind of job ended', () => {
    expect(databaseOutcome(databaseJob(1, 'backup_database', 'shop_main'))).toBe('Working…')
    expect(databaseOutcome(databaseJob(1, 'backup_database', 'shop_main', { state: 'cancelled' }))).toMatch(/^Cancelled/)
    expect(
      databaseOutcome(
        databaseJob(1, 'backup_database', 'shop_main', {
          state: 'failed',
          error: { error: 'backup_failed', message: 'pg_dump exited with status 1' },
        }),
      ),
    ).toBe('pg_dump exited with status 1')

    const ok = (kind: Parameters<typeof databaseJob>[1], result: unknown) =>
      databaseOutcome(databaseJob(1, kind, 'shop_main', { state: 'succeeded', result } as never))
    expect(
      ok('create_database', {
        kind: 'create_database',
        database: { name: 'shop_main', language: 'en_US', demo: false, login: 'admin', modules: ['sale'] },
      }),
    ).toBe('Created shop_main with sale. Sign in as “admin”.')
    expect(
      ok('restore_database', {
        kind: 'restore_database',
        database: { name: 'shop_main', format: 'zip', neutralized: true, new_uuid: true, filestore_files: 4 },
      }),
    ).toBe('Restored shop_main (neutralized, new UUID, 4 filestore files).')
    expect(
      ok('delete_database', {
        kind: 'delete_database',
        database: { name: 'shop_main', filestore_removed: true },
        snapshot: snapshot('shop_main__snap_20261126090500'),
      }),
    ).toBe('Deleted shop_main. Snapshot shop_main__snap_20261126090500 can bring it back.')
    expect(
      ok('duplicate_database', {
        kind: 'duplicate_database',
        database: { name: 'shop_test', source: 'shop_main', neutralized: true, filestore_copied: true },
        stopped: [],
        restarted: [],
      }),
    ).toBe('Copied shop_main to shop_test (neutralized).')
    expect(ok('backup_database', { kind: 'backup_database', backup: backup('shop_main_20261126-090500.zip') })).toBe(
      'Saved shop_main_20261126-090500.zip (2.0 KiB).',
    )
    expect(
      ok('snapshot_database', {
        kind: 'snapshot_database',
        snapshot: snapshot('shop_main__snap_20261126090500'),
        stopped: [],
        restarted: [],
      }),
    ).toBe('Took snapshot shop_main__snap_20261126090500.')
    expect(
      ok('restore_snapshot', {
        kind: 'restore_snapshot',
        database: { database: 'shop_main', snapshot: 'shop_main__snap_20261126090500', filestore: true },
        stopped: [],
        restarted: [],
      }),
    ).toBe('Restored shop_main from shop_main__snap_20261126090500.')
  })
})

describe('projectDatabaseJobs', () => {
  it('keeps the database jobs of one project', () => {
    const mine = databaseJob(1, 'backup_database', 'shop_main')
    const other = { ...databaseJob(2, 'backup_database', 'crm_main'), project: 'crm' }
    const setup: Job = { id: 3, kind: 'setup', project: 'shop', state: 'running', started_at: 1, steps: [], output: [], output_dropped: 0 }
    expect(isDatabaseJob(setup)).toBe(false)
    expect(projectDatabaseJobs([mine, other, setup], 'shop').map((job) => job.id)).toEqual([1])
  })
})
