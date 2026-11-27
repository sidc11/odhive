<script lang="ts" module>
  import type { ApiClient } from '../api/client'

  export type PostgresLogApi = Pick<ApiClient, 'postgresLog' | 'putPostgresLog'>
</script>

<script lang="ts">
  import { untrack } from 'svelte'
  import LogView from './LogView.svelte'
  import { PostgresLogStream } from './postgres.svelte'

  let {
    project,
    api,
    interval,
  }: {
    project: string
    api: PostgresLogApi
    /** Poll interval (ms), for tests. */
    interval?: number
  } = $props()

  // The parent keys this component by project, so the props never change.
  const stream = untrack(() => new PostgresLogStream((query) => api.postgresLog(project, query), { interval }))

  $effect(() => {
    stream.start()
    return () => stream.stop()
  })

  const info = $derived(stream.info)
  /** Value of the "other file" choice in the picker (paths start with `/`). */
  const OTHER = 'other'

  let choosing = $state(false)
  let choice = $state('')
  let custom = $state('')
  let saving = $state(false)
  let saveError = $state<string | null>(null)

  function startChoosing(): void {
    const configured = info?.configured ?? ''
    const known = configured === '' || (info?.found ?? []).includes(configured)
    choice = known ? configured : OTHER
    custom = known ? '' : configured
    saveError = null
    choosing = true
  }

  async function save(event: SubmitEvent): Promise<void> {
    event.preventDefault()
    const path = choice === OTHER ? custom.trim() : choice
    if (choice === OTHER && path === '') {
      saveError = 'Enter the absolute path of a *.log file.'
      return
    }
    saving = true
    saveError = null
    try {
      await api.putPostgresLog(project, { path: path === '' ? null : path })
      choosing = false
      stream.restart()
    } catch (error) {
      saveError = error instanceof Error ? error.message : String(error)
    } finally {
      saving = false
    }
  }

  const small =
    'inline-flex min-h-8 items-center justify-center rounded-md border border-line px-2 text-xs font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const field = 'min-h-8 w-full min-w-0 rounded border border-line bg-surface-raised px-2 font-mono text-xs'
</script>

<div class="flex flex-col gap-2">
  <div class="flex flex-wrap items-center gap-2 text-sm">
    <span class="text-fg-muted">{info?.source === 'journal' ? 'Journal' : 'File'}</span>
    {#if info?.source === 'journal' && info.unit}
      <span class="min-w-0 font-mono text-xs break-all" data-testid="postgres-log-path">{info.unit}</span>
      <span class="text-xs text-fg-muted">(systemd journal)</span>
    {:else if info?.path}
      <span class="min-w-0 font-mono text-xs break-all" data-testid="postgres-log-path">{info.path}</span>
      <span class="text-xs text-fg-muted">{info.configured ? '(chosen)' : '(found)'}</span>
    {:else if info}
      <span class="text-xs text-fg-muted">none</span>
    {/if}
    {#if !choosing && info}
      <button type="button" class={small} onclick={startChoosing}>Change…</button>
    {/if}
  </div>

  {#if choosing}
    <form class="flex flex-col gap-2 rounded-md border border-line p-3" onsubmit={save} aria-label="PostgreSQL log file">
      <label class="flex flex-col gap-1 text-sm">
        <span>Log file</span>
        <select class={field} onchange={(event) => (choice = event.currentTarget.value)}>
          <option value="" selected={choice === ''}>Find automatically</option>
          {#each info?.found ?? [] as path (path)}
            <option value={path} selected={choice === path}>{path}</option>
          {/each}
          <option value={OTHER} selected={choice === OTHER}>Other file…</option>
        </select>
      </label>
      {#if choice === OTHER}
        <label class="flex flex-col gap-1 text-sm">
          <span>Path</span>
          <input
            bind:value={custom}
            type="text"
            class={field}
            placeholder="/var/log/postgresql/postgresql-16-main.log"
            spellcheck="false"
            autocomplete="off"
          />
        </label>
      {/if}
      {#if saveError}
        <p class="text-sm text-state-crashed" role="alert">{saveError}</p>
      {/if}
      <div class="flex gap-2">
        <button type="submit" class="{small} border-transparent bg-accent text-accent-fg hover:bg-accent" disabled={saving}>Save</button>
        <button type="button" class={small} onclick={() => (choosing = false)} disabled={saving}>Cancel</button>
      </div>
    </form>
  {/if}

  {#if info && info.status !== 'ok'}
    <div class="rounded-md border border-line bg-surface-sunken px-3 py-2 text-sm" role="status">
      <p class="font-medium">{info.message}</p>
      {#if info.hint}
        <p class="mt-1 text-fg-muted">{info.hint}</p>
      {/if}
    </div>
  {/if}
  {#if stream.error}
    <p class="text-sm text-state-crashed" role="alert">Could not read the log: {stream.error}</p>
  {/if}

  <div class="flex h-[28rem] min-h-0 flex-col">
    <LogView
      {stream}
      label={`PostgreSQL log of ${project}`}
      source={{ project, instance: 'postgresql' }}
      showSources={false}
      idle="No PostgreSQL log to show."
    />
  </div>
</div>
