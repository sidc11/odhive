<script lang="ts">
  import LogPanel from '../lib/logs/LogPanel.svelte'
  import { routeHref, router } from '../lib/router.svelte'
  import { projects, settingsDrawer } from '../lib/stores'
  import { instanceState, STATE_LABELS } from '../lib/status'
  import StatusDot from '../lib/shell/StatusDot.svelte'

  let { project, instance }: { project: string; instance: string } = $props()

  const view = $derived(projects.find(project)?.instances.find((candidate) => candidate.id === instance))
</script>

<!-- The page fills the viewport below the top bar; the log takes the rest. -->
<div class="flex h-[calc(100dvh-7rem)] min-h-96 flex-col md:h-[calc(100dvh-8.5rem)]">
  <div class="flex flex-wrap items-center gap-3">
    <h1 class="min-w-0 flex-1 truncate text-2xl font-semibold">{instance}</h1>
    {#if view}
      <a
        href={routeHref({ name: 'logs', panes: [{ project, instance }] })}
        class="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken"
        title="Follow this log next to others"
        onclick={router.link}>Split view</a
      >
      <button
        type="button"
        class="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken"
        onclick={() => settingsDrawer.show(project, instance)}>Settings</button
      >
    {/if}
  </div>

  {#if view}
    {@const state = instanceState(view)}
    <p class="mt-2 flex items-center gap-2 text-sm">
      <StatusDot {state} />
      <span aria-hidden="true">{STATE_LABELS[state]}</span>
      <span class="text-fg-muted">· database {view.db}</span>
    </p>
    <h2 class="sr-only">Log</h2>
    <div class="mt-4 flex min-h-0 flex-1 flex-col">
      {#key `${project}/${instance}`}
        <LogPanel {project} {instance} />
      {/key}
    </div>
  {:else if projects.state.status === 'ready'}
    <p class="mt-2 text-sm text-fg-muted">No instance “{instance}” in project “{project}”.</p>
  {/if}
</div>
