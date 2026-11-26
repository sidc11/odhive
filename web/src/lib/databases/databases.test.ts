import { describe, expect, it, vi } from 'vitest'
import { backup, database, databaseJob, databaseList, routedFetch, snapshot } from '../../test-fixtures'
import { ApiClient, type Upload, type Uploader } from '../api/client'
import { JobsStore } from '../jobs.svelte'
import { DatabasesStore, modulesProblem, newNameProblem, parseModules } from './databases.svelte'

const LISTS = {
  'GET /projects/shop/databases': [200, databaseList([database('shop_main')])],
  'GET /projects/shop/backups': [200, [backup('shop_main_20261126-090500.zip')]],
  'GET /projects/shop/snapshots': [200, [snapshot('shop_main__snap_20261126090500')]],
} satisfies Record<string, [number, unknown?]>

function setup(routes: Record<string, [number, unknown?]> = {}, uploader?: Uploader, jobs = new JobsStore()) {
  const { fetcher, calls } = routedFetch({ ...LISTS, ...routes })
  const store = new DatabasesStore(new ApiClient(fetcher, uploader), jobs, 'shop')
  return { store, calls, jobs }
}

describe('form rules', () => {
  it('checks new database names like Odoo does', () => {
    expect(newNameProblem('shop_main')).toBeNull()
    expect(newNameProblem('a.b-c')).toBeNull()
    expect(newNameProblem('')).toBe('Enter a name.')
    for (const bad of ['a', '_shop', 'shop main', 'shop;drop', "o'neil", '-x', 'é'])
      expect(newNameProblem(bad), bad).not.toBeNull()
    expect(newNameProblem('a'.repeat(64))).toBe('At most 63 characters.')
  })

  it('reads module names from free text, once each', () => {
    expect(parseModules(' sale, stock\nsale  crm,')).toEqual(['sale', 'stock', 'crm'])
    expect(modulesProblem(['sale', 'Stock', 'a-b'])).toBe('Not a module name: Stock, a-b')
    expect(modulesProblem(['sale_management'])).toBeNull()
  })
})

describe('DatabasesStore', () => {
  it('loads databases, backups and snapshots; a failing list does not hide the others', async () => {
    const { store } = setup({ 'GET /projects/shop/snapshots': [503, { error: 'postgres_unavailable', message: 'down' }] })
    await store.load()
    expect(store.databases).toMatchObject({ status: 'ready', value: { databases: [{ name: 'shop_main' }] } })
    expect(store.backups).toMatchObject({ status: 'ready' })
    expect(store.snapshots).toEqual({ status: 'error', message: 'down' })
    expect(store.find('shop_main')?.name).toBe('shop_main')
  })

  it('turns the dialog into the progress of the job it starts', async () => {
    const job = databaseJob(7, 'delete_database', 'shop_main')
    const { store, calls, jobs } = setup({ 'DELETE /projects/shop/databases/shop_main': [202, job] })
    store.open({ kind: 'delete', database: database('shop_main') })
    expect(await store.delete('shop_main', 'shop_main', true)).toBe(true)
    expect(calls.at(-1)).toMatchObject({ method: 'DELETE', body: { confirm: 'shop_main', snapshot: true } })
    expect(store.dialog).toEqual({ kind: 'job', id: 7 })
    expect(store.busy).toBe(false)
    expect(jobs.get(7)?.kind).toBe('delete_database')
  })

  it('keeps a refused request in the dialog', async () => {
    const { store } = setup({
      'POST /projects/shop/databases/shop_main/duplicate': [409, { error: 'database_in_use', message: 'main runs on it' }],
    })
    const dialog = { kind: 'duplicate', database: database('shop_main') } as const
    store.open(dialog)
    expect(await store.duplicate('shop_main', { name: 'shop_copy', neutralize: false, stop_instances: false })).toBe(false)
    expect(store.dialog).toEqual(dialog)
    expect(store.error?.code).toBe('database_in_use')
    expect(store.error?.message).toBe('main runs on it')
    expect(store.busy).toBe(false)
  })

  it('uploads a restore with its options and reports progress', async () => {
    const uploads: Upload[] = []
    const seen: number[] = []
    const uploader: Uploader = async (upload) => {
      uploads.push(upload)
      upload.onprogress?.({ sent: 2, total: 4 })
      seen.push(store.upload?.sent ?? -1)
      return { status: 202, text: JSON.stringify(databaseJob(9, 'restore_database', 'shop_copy')) }
    }
    const { store } = setup({}, uploader)
    store.open({ kind: 'restore' })
    const file = new Blob(['PK'])
    expect(await store.restore(file, { name: 'shop_copy', neutralize: false, copy: true })).toBe(true)
    expect(uploads[0]?.url).toBe('/api/v1/projects/shop/databases/restore?name=shop_copy&neutralize=false&copy=true')
    expect(uploads[0]?.headers).toMatchObject({ 'X-OdHive': '1', 'Content-Type': 'application/octet-stream' })
    expect(uploads[0]?.body).toBe(file)
    expect(seen).toEqual([2])
    expect(store.upload).toBeNull()
    expect(store.dialog).toEqual({ kind: 'job', id: 9 })
  })

  it('stops the upload when the dialog closes', async () => {
    let signal: AbortSignal | undefined
    const uploader: Uploader = (upload) =>
      new Promise((_, reject) => {
        signal = upload.signal
        upload.signal?.addEventListener('abort', () => reject(new Error('aborted')))
      })
    const { store } = setup({}, uploader)
    store.open({ kind: 'restore' })
    const pending = store.restore(new Blob(['PK']), { name: 'shop_copy', neutralize: true, copy: true })
    store.close()
    expect(signal?.aborted).toBe(true)
    expect(await pending).toBe(false)
    expect(store.dialog).toBeNull()
    expect(store.error).toBeNull()
  })

  it('deletes a snapshot without a job, then closes and reloads', async () => {
    const { store, calls } = setup({ 'DELETE /projects/shop/snapshots/shop_main__snap_20261126090500': [204] })
    store.open({ kind: 'delete_snapshot', snapshot: snapshot('shop_main__snap_20261126090500') })
    expect(await store.deleteSnapshot('shop_main__snap_20261126090500')).toBe(true)
    expect(store.dialog).toBeNull()
    await vi.waitFor(() => expect(calls.filter((c) => c.path === '/projects/shop/snapshots')).toHaveLength(1))
  })

  it('reloads once for each database job of the project that ends, also ones started elsewhere', async () => {
    const jobs = new JobsStore()
    jobs.upsert(databaseJob(1, 'backup_database', 'shop_main', { state: 'succeeded' }))
    const { store, calls } = setup({}, undefined, jobs)
    const listCalls = () => calls.filter((c) => c.path === '/projects/shop/databases').length

    store.sync()
    expect(listCalls()).toBe(0) // ended before the page opened

    jobs.upsert(databaseJob(2, 'snapshot_database', 'shop_main'))
    jobs.upsert({ ...databaseJob(3, 'backup_database', 'crm_main', { state: 'succeeded' }), project: 'crm' })
    store.sync()
    expect(listCalls()).toBe(0)
    expect(store.running.map((job) => job.id)).toEqual([2])
    expect(store.jobOn('shop_main')?.id).toBe(2)
    expect(store.jobOn('other')).toBeUndefined()

    jobs.upsert(databaseJob(2, 'snapshot_database', 'shop_main', { state: 'succeeded' }))
    store.sync()
    store.sync()
    expect(listCalls()).toBe(1)
  })

  it('counts the source of a running copy as busy', () => {
    const jobs = new JobsStore()
    jobs.upsert(databaseJob(4, 'duplicate_database', 'shop_copy'))
    const { store } = setup({}, undefined, jobs)
    expect(store.jobOn('shop_main')?.id).toBe(4)
    expect(store.jobOn('shop_copy')?.id).toBe(4)
  })

  it('links backups for download below the API', () => {
    const { store } = setup()
    expect(store.backupUrl('a b.zip')).toBe('/api/v1/projects/shop/backups/a%20b.zip')
  })
})
