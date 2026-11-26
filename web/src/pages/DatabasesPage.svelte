<script lang="ts">
  import { DatabasesStore, type DatabasesApi, type Dialog } from '../lib/databases/databases.svelte'
  import DatabaseDialog from '../lib/databases/DatabaseDialog.svelte'
  import { isDatabaseJob, TITLES } from '../lib/databases/progress'
  import { formatBytes, formatTime } from '../lib/format'
  import type { JobsStore } from '../lib/jobs.svelte'
  import { routeHref, router } from '../lib/router.svelte'
  import { api as defaultApi, jobs as defaultJobs } from '../lib/stores'

  let {
    project,
    api = defaultApi,
    jobs = defaultJobs,
  }: {
    project: string
    api?: DatabasesApi
    jobs?: Pick<JobsStore, 'jobs' | 'get' | 'upsert' | 'finished'>
  } = $props()

  // The page is keyed by project (App.svelte), so one store per mount.
  // svelte-ignore state_referenced_locally
  const store = new DatabasesStore(api, jobs, project)
  void store.load()

  // Lists reload when a database job of the project ends, wherever it started.
  $effect(() => {
    for (const job of jobs.jobs) void job.state
    store.sync()
  })

  const list = $derived(store.databases.status === 'ready' ? store.databases.value : null)
  const pgError = $derived(list?.error ?? null)
  const running = $derived(store.running)

  function open(dialog: Dialog): void {
    store.open(dialog)
  }

  const button =
    'inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const small =
    'inline-flex min-h-8 items-center justify-center rounded-md border border-line px-2 text-xs font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const primary = `${button} border-transparent bg-accent text-accent-fg hover:bg-accent hover:opacity-90`
  const th = 'px-3 py-2 text-left text-xs font-semibold tracking-wide text-fg-muted uppercase'
  const td = 'px-3 py-2 align-top'
</script>

<div class="flex flex-col gap-1">
  <div class="flex flex-wrap items-center justify-between gap-2">
    <h1 class="text-2xl font-semibold">Databases</h1>
    <div class="flex flex-wrap gap-2">
      <button type="button" class={button} onclick={() => open({ kind: 'restore' })} disabled={pgError !== null}>
        Restore a backup…
      </button>
      <button type="button" class={primary} onclick={() => open({ kind: 'create' })} disabled={pgError !== null}>
        New database…
      </button>
    </div>
  </div>
</div>

{#if running.length > 0}
  <section aria-label="Running jobs" class="mt-4 flex flex-col gap-2">
    {#each running as job (job.id)}
      {#if isDatabaseJob(job)}
        <div class="flex flex-wrap items-center gap-3 rounded-lg border border-line bg-surface-raised px-4 py-2">
          <span class="size-2.5 shrink-0 animate-pulse rounded-full bg-state-starting" aria-hidden="true"></span>
          <span class="min-w-0 flex-1 text-sm">
            <span class="font-medium">{TITLES[job.kind]}</span>
            <span class="font-mono text-fg-muted">{job.database}</span>
          </span>
          <button type="button" class={small} onclick={() => open({ kind: 'job', id: job.id })}>Show progress</button>
        </div>
      {/if}
    {/each}
  </section>
{/if}

{#if store.databases.status === 'loading'}
  <p class="mt-6 text-sm text-fg-muted">Loading databases…</p>
{:else if store.databases.status === 'error'}
  <div class="mt-6 flex flex-col items-start gap-3">
    <p class="text-sm text-state-crashed" role="alert">{store.databases.message}</p>
    <button type="button" class={button} onclick={() => store.load()}>Try again</button>
  </div>
{:else if list}
  {#if pgError}
    <div class="mt-6 flex flex-col gap-2 rounded-lg border border-state-crashed/40 bg-surface-raised p-4" role="alert">
      <p class="font-medium">{pgError.summary}</p>
      {#each pgError.details as detail, index (index)}<p class="text-sm break-all text-fg-muted">{detail}</p>{/each}
      {#if pgError.fix}<p class="text-sm">{pgError.fix.text}</p>{/if}
      <div class="flex flex-wrap gap-2">
        <a href={routeHref({ name: 'environment' })} class={button} onclick={router.link}>Open the environment check</a>
        <button type="button" class={button} onclick={() => store.load()}>Try again</button>
      </div>
    </div>
  {/if}

  <section aria-labelledby="databases-heading" class="mt-6">
    <h2 id="databases-heading" class="sr-only">Databases</h2>
    {#if list.filter && list.hidden > 0}
      <p class="mb-2 text-sm text-fg-muted">
        The project's dbfilter <code class="font-mono">{list.filter}</code> hides {list.hidden}
        {list.hidden === 1 ? 'database' : 'databases'}.
      </p>
    {/if}
    {#if list.filter_note}<p class="mb-2 text-sm text-fg-muted">{list.filter_note}</p>{/if}

    {#if list.databases.length === 0 && !pgError}
      <div class="rounded-lg border border-dashed border-line p-6 text-center">
        <p class="text-sm text-fg-muted">No databases yet. Create one, or restore a backup.</p>
      </div>
    {:else if list.databases.length > 0}
      <div class="relative overflow-x-auto rounded-lg border border-line bg-surface-raised">
        <table class="w-full text-sm">
          <thead class="border-b border-line">
            <tr>
              <th scope="col" class={th}>Name</th>
              <th scope="col" class={th}>Odoo</th>
              <th scope="col" class="{th} text-right">Size</th>
              <th scope="col" class="{th} hidden md:table-cell">Last change</th>
              <th scope="col" class={th}>Used by</th>
              <th scope="col" class={th}><span class="sr-only">Actions</span></th>
            </tr>
          </thead>
          <tbody class="divide-y divide-line">
            {#each list.databases as db (db.name)}
              {@const busy = store.jobOn(db.name)}
              {@const usable = db.valid_name && !busy}
              <tr data-database={db.name}>
                <th scope="row" class="{td} text-left font-medium">
                  <span class="font-mono break-words">{db.name}</span>
                  {#if busy && isDatabaseJob(busy)}
                    <span class="block text-xs font-normal text-state-starting">{TITLES[busy.kind]}…</span>
                  {:else if !db.valid_name}
                    <span class="block text-xs font-normal text-fg-muted">Name OdHive does not act on</span>
                  {/if}
                  {#if db.error}<span class="block text-xs font-normal text-state-crashed">{db.error}</span>{/if}
                </th>
                <td class={td}>
                  {#if db.odoo}{db.odoo_version ?? db.odoo_series ?? 'Odoo'}{:else}<span class="text-fg-muted">Not Odoo</span>{/if}
                </td>
                <td class="{td} text-right whitespace-nowrap">{db.size_bytes == null ? '—' : formatBytes(db.size_bytes)}</td>
                <td class="{td} hidden whitespace-nowrap md:table-cell">{db.last_modified ? formatTime(db.last_modified) : '—'}</td>
                <td class={td}>
                  {#if db.instances.length > 0}
                    {#each db.instances as instance, index (instance)}{#if index > 0}, {/if}<a
                        href={routeHref({ name: 'instance', project, instance })}
                        class="underline-offset-2 hover:underline"
                        onclick={router.link}>{instance}</a
                      >{/each}
                  {:else}<span class="text-fg-muted">—</span>{/if}
                </td>
                <td class={td}>
                  <div class="flex flex-wrap justify-end gap-1" role="group" aria-label="Actions for {db.name}">
                    <button type="button" class={small} disabled={!usable} onclick={() => open({ kind: 'backup', database: db })}>Back up</button>
                    <button type="button" class={small} disabled={!usable} onclick={() => open({ kind: 'snapshot', database: db })}>Snapshot</button>
                    <button type="button" class={small} disabled={!usable} onclick={() => open({ kind: 'duplicate', database: db })}>Duplicate</button>
                    <button
                      type="button"
                      class="{small} text-state-crashed"
                      disabled={!usable}
                      title={db.instances.length > 0 ? 'Stop the instances using it first' : undefined}
                      onclick={() => open({ kind: 'delete', database: db })}>Delete</button
                    >
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>

  <section aria-labelledby="snapshots-heading" class="mt-8">
    <h2 id="snapshots-heading" class="text-lg font-semibold">Snapshots</h2>
    <p class="text-sm text-fg-muted">Instant restore points, kept on the PostgreSQL server.</p>
    {#if store.snapshots.status === 'error'}
      <p class="mt-2 text-sm text-state-crashed">{store.snapshots.message}</p>
    {:else if store.snapshots.status === 'ready'}
      {#if store.snapshots.value.length === 0}
        <p class="mt-2 text-sm text-fg-muted">No snapshots.</p>
      {:else}
        <ul class="mt-2 flex flex-col divide-y divide-line rounded-lg border border-line bg-surface-raised">
          {#each store.snapshots.value as snap (snap.name)}
            {@const busy = store.jobOn(snap.database)}
            <li class="flex flex-wrap items-center gap-3 px-4 py-2" data-snapshot={snap.name}>
              <div class="min-w-0 flex-1">
                <p class="text-sm font-medium"><span class="font-mono">{snap.database}</span> · {formatTime(snap.created_at, { seconds: true })}</p>
                <p class="text-xs text-fg-muted">
                  {#if snap.broken}<span class="text-state-crashed">Its PostgreSQL copy is gone; it can only be deleted.</span>{:else}
                    {snap.size == null ? 'Size unknown' : formatBytes(snap.size)} · {snap.filestore ? 'with filestore' : 'no filestore'}
                  {/if}
                </p>
              </div>
              <div class="flex gap-1" role="group" aria-label="Actions for snapshot {snap.name}">
                <button
                  type="button"
                  class={small}
                  disabled={snap.broken || busy !== undefined}
                  onclick={() => open({ kind: 'restore_snapshot', snapshot: snap })}>Restore</button
                >
                <button
                  type="button"
                  class="{small} text-state-crashed"
                  disabled={busy !== undefined}
                  onclick={() => open({ kind: 'delete_snapshot', snapshot: snap })}>Delete</button
                >
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    {/if}
  </section>

  <section aria-labelledby="backups-heading" class="mt-8">
    <h2 id="backups-heading" class="text-lg font-semibold">Backups</h2>
    <p class="text-sm text-fg-muted">Files in the project's backups folder.</p>
    {#if store.backups.status === 'error'}
      <p class="mt-2 text-sm text-state-crashed">{store.backups.message}</p>
    {:else if store.backups.status === 'ready'}
      {#if store.backups.value.length === 0}
        <p class="mt-2 text-sm text-fg-muted">No backups.</p>
      {:else}
        <ul class="mt-2 flex flex-col divide-y divide-line rounded-lg border border-line bg-surface-raised">
          {#each store.backups.value as backup (backup.file)}
            <li class="flex flex-wrap items-center gap-3 px-4 py-2" data-backup={backup.file}>
              <div class="min-w-0 flex-1">
                <p class="font-mono text-sm break-all">{backup.file}</p>
                <p class="text-xs text-fg-muted">
                  {backup.format === 'zip' ? 'Odoo backup' : 'pg_dump archive'} · {formatBytes(backup.size)} · {formatTime(backup.modified_at)}
                </p>
              </div>
              <div class="flex gap-1" role="group" aria-label="Actions for backup {backup.file}">
                <a href={store.backupUrl(backup.file)} class={small} download={backup.file}>Download</a>
                <button type="button" class="{small} text-state-crashed" onclick={() => open({ kind: 'delete_backup', backup })}>Delete</button>
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    {/if}
  </section>
{/if}

{#if store.dialog}
  {#key store.dialog}
    <DatabaseDialog {store} dialog={store.dialog} />
  {/key}
{/if}
