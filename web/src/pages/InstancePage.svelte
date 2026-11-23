<script lang="ts">
  import { projects } from '../lib/projects.svelte'
  import { instanceState, STATE_LABELS } from '../lib/status'
  import StatusDot from '../lib/shell/StatusDot.svelte'

  let { project, instance }: { project: string; instance: string } = $props()

  const view = $derived(projects.find(project)?.instances.find((candidate) => candidate.id === instance))
</script>

<h1 class="truncate text-2xl font-semibold">{instance}</h1>

{#if view}
  {@const state = instanceState(view)}
  <p class="mt-2 flex items-center gap-2 text-sm">
    <StatusDot {state} />
    <span aria-hidden="true">{STATE_LABELS[state]}</span>
    <span class="text-fg-muted">· database {view.db}</span>
  </p>
{:else if projects.state.status === 'ready'}
  <p class="mt-2 text-sm text-fg-muted">No instance “{instance}” in project “{project}”.</p>
{/if}
