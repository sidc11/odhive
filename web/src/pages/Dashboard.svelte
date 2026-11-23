<script lang="ts">
  import { projects } from '../lib/stores'
  import { instanceState, isUp } from '../lib/status'

  const totals = $derived.by(() => {
    if (projects.state.status !== 'ready') return null
    const instances = projects.state.projects.flatMap((project) => project.instances)
    return {
      projects: projects.state.projects.length,
      instances: instances.length,
      up: instances.filter((view) => isUp(instanceState(view))).length,
    }
  })
</script>

<h1 class="text-2xl font-semibold">Dashboard</h1>

{#if totals}
  {#if totals.projects === 0}
    <div class="mt-6 rounded-lg border border-dashed border-line bg-surface-raised p-6 text-center">
      <p class="font-medium">No projects yet</p>
      <p class="mt-1 text-sm text-fg-muted">
        Add one with <code class="rounded bg-surface-sunken px-1">odhive project add</code> for now.
      </p>
    </div>
  {:else}
    <p class="mt-2 text-sm text-fg-muted">
      {totals.projects}
      {totals.projects === 1 ? 'project' : 'projects'} · {totals.up} of {totals.instances}
      {totals.instances === 1 ? 'instance' : 'instances'} running
    </p>
  {/if}
{/if}
