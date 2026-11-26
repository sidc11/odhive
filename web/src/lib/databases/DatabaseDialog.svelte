<script lang="ts">
  import Modal from '../shell/Modal.svelte'
  import { formatBytes, formatTime } from '../format'
  import type { DatabasesStore, Dialog } from './databases.svelte'
  import { modulesProblem, newNameProblem, parseModules } from './databases.svelte'
  import JobProgress from './JobProgress.svelte'
  import { isDatabaseJob, TITLES } from './progress'

  let { store, dialog }: { store: DatabasesStore; dialog: Dialog } = $props()

  const formId = 'database-dialog-form'
  /** A submit was tried: problems of untouched fields show from then on. */
  let tried = $state(false)

  // Create
  let name = $state('')
  let language = $state('')
  let country = $state('')
  let login = $state('')
  let password = $state('')
  let demo = $state(false)
  let modulesText = $state('')
  // Restore
  let file = $state<File | null>(null)
  let neutralize = $state(true)
  let copy = $state(true)
  // Delete
  let confirm = $state('')
  let takeSnapshot = $state(true)
  // Backup
  let format = $state<'zip' | 'dump'>('zip')
  // Duplicate, snapshot
  let stopInstances = $state(false)

  // Defaults that depend on the dialog's subject, set once.
  // svelte-ignore state_referenced_locally
  if (dialog.kind === 'duplicate') {
    name = `${dialog.database.name}_copy`.slice(0, 63)
    neutralize = false
  } else if (dialog.kind === 'delete') {
    // A snapshot name needs room for `__snap_<timestamp>`.
    takeSnapshot = dialog.database.name.length <= 42
  }

  const modules = $derived(parseModules(modulesText))
  const nameProblem = $derived(newNameProblem(name))
  const modulesError = $derived(modulesProblem(modules))

  const error = $derived(store.error)
  const job = $derived.by(() => {
    if (dialog.kind !== 'job') return undefined
    const found = store.jobs.get(dialog.id)
    return found && isDatabaseJob(found) ? found : undefined
  })

  /** The database whose running instances a request may stop. */
  const subject = $derived.by(() => {
    switch (dialog.kind) {
      case 'duplicate':
      case 'snapshot':
        return store.find(dialog.database.name) ?? dialog.database
      case 'restore_snapshot':
        return store.find(dialog.snapshot.database)
      default:
        return undefined
    }
  })
  const users = $derived(subject?.instances ?? [])
  const offerStop = $derived(users.length > 0 || error?.code === 'database_in_use')

  const title = $derived.by(() => {
    switch (dialog.kind) {
      case 'create':
        return 'New database'
      case 'restore':
        return 'Restore a backup'
      case 'delete':
        return 'Delete database'
      case 'duplicate':
        return 'Duplicate database'
      case 'backup':
        return 'Back up database'
      case 'snapshot':
        return 'Take a snapshot'
      case 'restore_snapshot':
        return 'Restore snapshot'
      case 'delete_snapshot':
        return 'Delete snapshot'
      case 'delete_backup':
        return 'Delete backup'
      case 'job':
        return job ? TITLES[job.kind] : 'Database job'
    }
  })

  const subtitle = $derived.by(() => {
    switch (dialog.kind) {
      case 'delete':
      case 'duplicate':
      case 'backup':
      case 'snapshot':
        return dialog.database.name
      case 'restore_snapshot':
      case 'delete_snapshot':
        return dialog.snapshot.name
      case 'delete_backup':
        return dialog.backup.file
      case 'job':
        return job ? job.database : undefined
      default:
        return `Project ${store.project}`
    }
  })

  /** A database name from a file name: `shop_2026-01-02.zip` → `shop_2026-01-02`. */
  function nameFromFile(fileName: string): string {
    const stem = fileName.replace(/\.(zip|dump|sql|backup|tar)$/i, '')
    const cleaned = stem.replace(/[^A-Za-z0-9_.-]+/g, '_').replace(/^[^A-Za-z0-9]+/, '')
    return cleaned.slice(0, 63)
  }

  function pick(event: Event): void {
    const input = event.currentTarget as HTMLInputElement
    file = input.files?.[0] ?? null
    if (file && name === '') name = nameFromFile(file.name)
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault()
    tried = true
    switch (dialog.kind) {
      case 'create':
        if (nameProblem || modulesError || password === '') return
        await store.create({
          name,
          password,
          demo,
          modules,
          language: language.trim() || null,
          country: country.trim() || null,
          login: login.trim() || null,
        })
        break
      case 'restore':
        if (nameProblem || !file) return
        await store.restore(file, { name, neutralize, copy })
        break
      case 'delete':
        if (confirm !== dialog.database.name) return
        await store.delete(dialog.database.name, confirm, takeSnapshot)
        break
      case 'duplicate':
        if (nameProblem) return
        await store.duplicate(dialog.database.name, { name, neutralize, stop_instances: stopInstances })
        break
      case 'backup':
        await store.backup(dialog.database.name, format)
        break
      case 'snapshot':
        await store.snapshot(dialog.database.name, stopInstances)
        break
      case 'restore_snapshot':
        await store.restoreSnapshot(dialog.snapshot.name, stopInstances)
        break
      case 'delete_snapshot':
        await store.deleteSnapshot(dialog.snapshot.name)
        break
      case 'delete_backup':
        await store.deleteBackup(dialog.backup.file)
        break
    }
  }

  const SUBMIT: Record<Exclude<Dialog['kind'], 'job'>, string> = {
    create: 'Create',
    restore: 'Upload and restore',
    delete: 'Delete',
    duplicate: 'Duplicate',
    backup: 'Back up',
    snapshot: 'Take snapshot',
    restore_snapshot: 'Restore',
    delete_snapshot: 'Delete snapshot',
    delete_backup: 'Delete backup',
  }
  const danger = $derived(
    dialog.kind === 'delete' ||
      dialog.kind === 'restore_snapshot' ||
      dialog.kind === 'delete_snapshot' ||
      dialog.kind === 'delete_backup',
  )

  const input =
    'min-h-9 w-full rounded-md border border-line bg-surface px-3 text-sm placeholder:text-fg-muted aria-invalid:border-state-crashed disabled:opacity-50'
  const button =
    'inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const primary = $derived(
    danger
      ? `${button} border-transparent bg-state-crashed text-surface hover:bg-state-crashed hover:opacity-90`
      : `${button} border-transparent bg-accent text-accent-fg hover:bg-accent hover:opacity-90`,
  )
  const check = 'mt-0.5 accent-accent'
  const field = 'flex flex-col gap-1'
  const label = 'text-sm font-medium'
  const help = 'text-sm text-fg-muted'
  const problem = 'text-sm text-state-crashed'
</script>

<Modal {title} {subtitle} onclose={() => store.close()} testid="database-dialog">
  {#if dialog.kind === 'job'}
    {#if job}
      <JobProgress {job} />
      {#if job.state === 'running'}
        <p class="mt-4 text-sm text-fg-muted">Closing this keeps the job running; it shows on the page until it ends.</p>
      {/if}
    {:else}
      <p class="text-sm text-fg-muted">This job is no longer known; the OdHive service may have restarted.</p>
    {/if}
    {#if error}<p class="{problem} mt-2" role="alert">{error.message}</p>{/if}
  {:else}
    <form id={formId} class="flex flex-col gap-4" onsubmit={submit} novalidate>
      {#if dialog.kind === 'create' || dialog.kind === 'restore' || dialog.kind === 'duplicate'}
        {#if dialog.kind === 'restore'}
          <div class={field}>
            <label for="db-file" class={label}>Backup file</label>
            <input
              id="db-file"
              type="file"
              class="text-sm file:mr-3 file:min-h-9 file:rounded-md file:border file:border-line file:bg-surface file:px-3 file:text-sm"
              accept=".zip,.dump,.sql,.backup,.tar,application/zip,application/sql"
              onchange={pick}
              aria-invalid={tried && !file}
              aria-describedby="db-file-help"
              disabled={store.busy}
            />
            <p id="db-file-help" class={help}>
              An Odoo backup (.zip with dump.sql and the filestore), a pg_dump archive or an SQL script. Restore only files
              you trust.
            </p>
            {#if tried && !file}<p class={problem}>Choose a file.</p>{/if}
          </div>
        {/if}
        <div class={field}>
          <label for="db-name" class={label}>{dialog.kind === 'duplicate' ? 'Name of the copy' : 'Database name'}</label>
          <input
            id="db-name"
            class="{input} font-mono"
            bind:value={name}
            autocomplete="off"
            spellcheck="false"
            maxlength="63"
            aria-invalid={Boolean((tried || name !== '') && nameProblem)}
            aria-describedby={(tried || name !== '') && nameProblem ? 'db-name-problem' : undefined}
            disabled={store.busy}
          />
          {#if (tried || name !== '') && nameProblem}<p id="db-name-problem" class={problem}>{nameProblem}</p>{/if}
        </div>
      {/if}

      {#if dialog.kind === 'create'}
        <div class="grid gap-4 sm:grid-cols-2">
          <div class={field}>
            <label for="db-language" class={label}>Language</label>
            <input id="db-language" class="{input} font-mono" bind:value={language} placeholder="en_US" autocomplete="off" spellcheck="false" />
          </div>
          <div class={field}>
            <label for="db-country" class={label}>Country</label>
            <input
              id="db-country"
              class="{input} font-mono"
              bind:value={country}
              placeholder="BE"
              maxlength="2"
              autocomplete="off"
              spellcheck="false"
              aria-describedby="db-country-help"
            />
            <p id="db-country-help" class={help}>Two letters; sets the company currency.</p>
          </div>
          <div class={field}>
            <label for="db-login" class={label}>Admin login</label>
            <input id="db-login" class={input} bind:value={login} placeholder="admin" autocomplete="off" spellcheck="false" />
          </div>
          <div class={field}>
            <label for="db-password" class={label}>Admin password</label>
            <input
              id="db-password"
              type="password"
              class={input}
              bind:value={password}
              autocomplete="new-password"
              aria-invalid={tried && password === ''}
              aria-describedby={tried && password === '' ? 'db-password-problem' : undefined}
            />
            {#if tried && password === ''}<p id="db-password-problem" class={problem}>Enter a password.</p>{/if}
          </div>
        </div>
        <div class={field}>
          <label for="db-modules" class={label}>Modules to install</label>
          <input
            id="db-modules"
            class="{input} font-mono"
            bind:value={modulesText}
            placeholder="sale_management, stock"
            autocomplete="off"
            spellcheck="false"
            aria-invalid={Boolean(modulesError)}
            aria-describedby="db-modules-help"
          />
          <p id="db-modules-help" class={modulesError ? problem : help}>
            {modulesError ?? 'Technical names, separated by commas or spaces. base is always installed.'}
          </p>
        </div>
        <label class="flex items-start gap-2 text-sm">
          <input type="checkbox" class={check} bind:checked={demo} />
          <span>Load demo data</span>
        </label>
      {/if}

      {#if dialog.kind === 'restore' || dialog.kind === 'duplicate'}
        <label class="flex items-start gap-2 text-sm">
          <input type="checkbox" class={check} bind:checked={neutralize} disabled={store.busy} />
          <span>
            <span class="block font-medium">Neutralize</span>
            <span class="block text-fg-muted">Turns off outgoing mail servers, scheduled actions and other links to the outside.</span>
          </span>
        </label>
      {/if}
      {#if dialog.kind === 'restore'}
        <label class="flex items-start gap-2 text-sm">
          <input type="checkbox" class={check} bind:checked={copy} disabled={store.busy} />
          <span>
            <span class="block font-medium">It is a copy</span>
            <span class="block text-fg-muted">Gives it a new database UUID. Clear this only for a database that was moved here.</span>
          </span>
        </label>
        {#if store.upload}
          <div class={field}>
            <label for="db-upload" class={label}>Uploading</label>
            <progress id="db-upload" class="w-full accent-accent" max={store.upload.total || 1} value={store.upload.sent}></progress>
            <p class={help}>{formatBytes(store.upload.sent)} of {formatBytes(store.upload.total)}</p>
          </div>
        {/if}
      {/if}

      {#if dialog.kind === 'delete'}
        <p class="text-sm">
          This drops <strong class="font-mono">{dialog.database.name}</strong> and removes its filestore.
          {#if !takeSnapshot}It cannot be undone.{/if}
        </p>
        <label class="flex items-start gap-2 text-sm">
          <input type="checkbox" class={check} bind:checked={takeSnapshot} />
          <span>
            <span class="block font-medium">Take a snapshot first</span>
            <span class="block text-fg-muted">A restore point to undo the delete with.</span>
          </span>
        </label>
        <div class={field}>
          <label for="db-confirm" class={label}>Type <span class="font-mono">{dialog.database.name}</span> to confirm</label>
          <input
            id="db-confirm"
            class="{input} font-mono"
            bind:value={confirm}
            autocomplete="off"
            spellcheck="false"
            aria-invalid={tried && confirm !== dialog.database.name}
          />
        </div>
      {/if}

      {#if dialog.kind === 'backup'}
        <fieldset class="flex flex-col gap-2">
          <legend class="mb-2 text-sm font-medium">Format</legend>
          <label class="flex items-start gap-2 text-sm">
            <input type="radio" name="backup-format" value="zip" class={check} bind:group={format} />
            <span>
              <span class="block font-medium">Odoo backup (.zip)</span>
              <span class="block text-fg-muted">The database, the filestore and a manifest, like Odoo's database manager.</span>
            </span>
          </label>
          <label class="flex items-start gap-2 text-sm">
            <input type="radio" name="backup-format" value="dump" class={check} bind:group={format} />
            <span>
              <span class="block font-medium">pg_dump archive (.dump)</span>
              <span class="block text-fg-muted">The database only, without the filestore.</span>
            </span>
          </label>
        </fieldset>
        <p class={help}>Running instances can keep running.</p>
      {/if}

      {#if dialog.kind === 'snapshot'}
        <p class="text-sm">
          Copies <span class="font-mono">{dialog.database.name}</span> and keeps its filestore, to come back to later. The
          database must not be in use while it is copied.
        </p>
      {/if}

      {#if dialog.kind === 'restore_snapshot'}
        <p class="text-sm">
          Replaces <strong class="font-mono">{dialog.snapshot.database}</strong> and its filestore with the snapshot taken
          {formatTime(dialog.snapshot.created_at, { seconds: true })}. Changes made since are lost. The snapshot is kept.
        </p>
      {/if}

      {#if dialog.kind === 'delete_snapshot'}
        <p class="text-sm">This removes the snapshot of <span class="font-mono">{dialog.snapshot.database}</span>. The database itself stays.</p>
      {/if}

      {#if dialog.kind === 'delete_backup'}
        <p class="text-sm">This removes the file from the project's backups folder.</p>
      {/if}

      {#if (dialog.kind === 'duplicate' || dialog.kind === 'snapshot' || dialog.kind === 'restore_snapshot') && offerStop}
        <label class="flex items-start gap-2 text-sm">
          <input type="checkbox" class={check} bind:checked={stopInstances} />
          <span>
            <span class="block font-medium">Stop the instances using it</span>
            <span class="block text-fg-muted">
              {#if users.length > 0}{users.join(', ')} {users.length === 1 ? 'runs' : 'run'} on it.{/if}
              Those OdHive runs start again afterwards; those run from a terminal stay stopped.
            </span>
          </span>
        </label>
      {/if}

      {#if error}<p class={problem} role="alert">{error.message}</p>{/if}
    </form>
  {/if}

  {#snippet footer()}
    {#if dialog.kind === 'job'}
      {#if job?.state === 'running'}
        <button type="button" class={button} onclick={() => store.cancel(job.id)}>Cancel job</button>
      {/if}
      <button type="button" class={button} onclick={() => store.close()} data-autofocus>Close</button>
    {:else}
      <button type="button" class={button} onclick={() => store.close()}>
        {store.upload ? 'Stop upload' : 'Cancel'}
      </button>
      <button
        type="submit"
        form={formId}
        class={primary}
        disabled={store.busy || (dialog.kind === 'delete' && confirm !== dialog.database.name)}
      >
        {store.busy ? 'Working…' : SUBMIT[dialog.kind]}
      </button>
    {/if}
  {/snippet}
</Modal>
