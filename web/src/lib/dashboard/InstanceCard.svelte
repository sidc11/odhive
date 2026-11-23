<script lang="ts">
  import type { InstanceUsage, InstanceView, Project } from '../api'
  import type { InstanceAction } from '../actions.svelte'
  import { formatBytes, formatCpu, formatUptime } from '../format'
  import { routeHref, router } from '../router.svelte'
  import StatusDot from '../shell/StatusDot.svelte'
  import { instanceState, isUp, STATE_LABELS } from '../status'
  import { exitSummary, gitSummary, odooUrl, portMove } from './card'

  let {
    project,
    view,
    usage,
    now,
    busy,
    onaction,
  }: {
    project: Project
    view: InstanceView
    usage: InstanceUsage | undefined
    /** Wall clock (ms) for the uptime. */
    now: number
    busy: InstanceAction | undefined
    onaction: (action: InstanceAction) => void
  } = $props()

  const state = $derived(instanceState(view))
  const port = $derived(view.live?.http_port ?? view.external?.http_port ?? view.http_port ?? null)
  const moved = $derived(portMove(view.live))
  const git = $derived(gitSummary(project.git))
  const exit = $derived(exitSummary(view.live?.status))
  const reachable = $derived((state === 'running' || state === 'external') && port !== null)
  const canStart = $derived(!isUp(state) && project.has_venv && !project.error)
  const canStop = $derived(state === 'starting' || state === 'running' || state === 'external')
  const canRestart = $derived(state === 'starting' || state === 'running')
  const headingId = $derived(`card-${project.name}-${view.id}`)

  const button =
    'inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
</script>

<article
  class="flex w-full flex-col gap-3 rounded-lg border border-line bg-surface-raised p-4"
  aria-labelledby={headingId}
  data-state={state}
>
  <header class="flex items-start gap-2">
    <span class="mt-2"><StatusDot {state} /></span>
    <div class="min-w-0 flex-1">
      <h3 id={headingId} class="truncate font-semibold">
        <a
          href={routeHref({ name: 'instance', project: project.name, instance: view.id })}
          class="hover:underline"
          onclick={router.link}>{project.name} / {view.id}</a
        >
      </h3>
      <p class="truncate text-sm text-fg-muted">
        {project.odoo_version ? `Odoo ${project.odoo_version}` : 'Odoo version unknown'}{project.enterprise
          ? ' Enterprise'
          : ''}
      </p>
    </div>
    <span class="shrink-0 rounded-full bg-surface-sunken px-2 py-0.5 text-xs font-medium" aria-hidden="true">
      {STATE_LABELS[state]}
    </span>
  </header>

  {#if exit}
    <p class="text-sm text-state-crashed">{exit}</p>
  {/if}

  <dl class="grid grid-cols-2 gap-x-4 gap-y-2 text-sm">
    <div class="min-w-0">
      <dt class="text-xs text-fg-muted">Port</dt>
      <dd class="flex flex-wrap items-center gap-1.5">
        <span class="tabular-nums">{port ?? 'auto'}</span>
        {#if moved}
          <span
            class="rounded bg-accent-soft px-1.5 py-0.5 text-xs text-fg"
            title={moved.reason}
            data-testid="port-moved">switched from {moved.from}</span
          >
        {/if}
      </dd>
    </div>
    <div class="min-w-0">
      <dt class="text-xs text-fg-muted">Database</dt>
      <dd class="truncate" title={view.live?.db ?? view.db}>{view.live?.db ?? view.db}</dd>
    </div>
    <div class="min-w-0">
      <dt class="text-xs text-fg-muted">Branch</dt>
      <dd class="truncate" title={git?.title}>{git?.label ?? '—'}</dd>
    </div>
    <div class="min-w-0">
      <dt class="text-xs text-fg-muted">Uptime</dt>
      <dd class="tabular-nums">{usage && isUp(state) ? formatUptime(usage.started_at, now) : '—'}</dd>
    </div>
    <div class="min-w-0">
      <dt class="text-xs text-fg-muted">CPU</dt>
      <dd class="tabular-nums">{usage && isUp(state) ? formatCpu(usage.cpu_percent) : '—'}</dd>
    </div>
    <div class="min-w-0">
      <dt class="text-xs text-fg-muted">Memory</dt>
      <dd class="tabular-nums" title={usage && usage.processes > 1 ? `${usage.processes} processes` : undefined}>
        {usage && isUp(state) ? formatBytes(usage.memory_bytes) : '—'}
      </dd>
    </div>
  </dl>

  {#if !project.has_venv && !isUp(state)}
    <p class="text-xs text-fg-muted">Set up the project's Python environment before starting it.</p>
  {/if}

  <div class="mt-auto flex flex-wrap items-center gap-2">
    {#if canStop}
      <button type="button" class={button} disabled={busy !== undefined} onclick={() => onaction('stop')}>
        <svg viewBox="0 0 24 24" class="size-4" fill="currentColor" aria-hidden="true"><rect x="6" y="6" width="12" height="12" rx="1.5" /></svg>
        {busy === 'stop' ? 'Stopping…' : 'Stop'}
      </button>
    {:else}
      <button
        type="button"
        class="{button} border-transparent bg-accent text-accent-fg hover:bg-accent hover:opacity-90"
        disabled={!canStart || busy !== undefined}
        onclick={() => onaction('start')}
      >
        <svg viewBox="0 0 24 24" class="size-4" fill="currentColor" aria-hidden="true"><path d="M8 5.5v13l10.5-6.5z" /></svg>
        {busy === 'start' ? 'Starting…' : 'Start'}
      </button>
    {/if}
    {#if canRestart}
      <button type="button" class={button} disabled={busy !== undefined} onclick={() => onaction('restart')}>
        <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"
          ><path d="M20 11a8 8 0 1 0-2.3 5.7" /><path d="M20 4v7h-7" /></svg
        >
        {busy === 'restart' ? 'Restarting…' : 'Restart'}
      </button>
    {/if}
    {#if reachable && port !== null}
      <span class="ml-auto flex gap-2">
        <a class={button} href={odooUrl(port)} target="_blank" rel="noopener noreferrer">
          Open<span class="sr-only"> {project.name} / {view.id} in a new tab</span>
        </a>
        <a class={button} href={odooUrl(port, true)} target="_blank" rel="noopener noreferrer" title="Open with ?debug=assets">
          Debug<span class="sr-only"> mode (assets) for {project.name} / {view.id}, in a new tab</span>
        </a>
      </span>
    {/if}
  </div>
</article>
