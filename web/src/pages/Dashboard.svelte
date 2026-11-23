<script lang="ts">
  import InstanceCard from '../lib/dashboard/InstanceCard.svelte'
  import { actions, projects, usage } from '../lib/stores'
  import { routeHref, router } from '../lib/router.svelte'
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

  // CPU, memory and uptime are polled while the dashboard is open.
  $effect(() => usage.start())
</script>

<h1 class="text-2xl font-semibold">Dashboard</h1>

{#if projects.state.status === 'ready' && totals}
  {#if totals.projects === 0}
    <div class="mt-6 rounded-lg border border-dashed border-line bg-surface-raised p-6 text-center">
      <p class="font-medium">No projects yet</p>
      <p class="mt-1 text-sm text-fg-muted">Create one to set up Odoo, its Python environment and a first instance.</p>
      <a
        href={routeHref({ name: 'new_project' })}
        class="mt-4 inline-flex min-h-9 items-center rounded-md bg-accent px-3 text-sm font-medium text-accent-fg hover:opacity-90"
        onclick={router.link}>New project</a
      >
    </div>
  {:else}
    <div class="mt-2 flex flex-wrap items-center gap-2">
      <p class="flex-1 text-sm text-fg-muted">
        {totals.projects}
        {totals.projects === 1 ? 'project' : 'projects'} · {totals.up} of {totals.instances}
        {totals.instances === 1 ? 'instance' : 'instances'} running
      </p>
      <a
        href={routeHref({ name: 'new_project' })}
        class="inline-flex min-h-9 items-center rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken"
        onclick={router.link}>New project</a
      >
    </div>
    <h2 class="sr-only">Instances</h2>
    <ul class="mt-4 grid grid-cols-1 gap-4 md:grid-cols-2 2xl:grid-cols-3">
      {#each projects.state.projects as project (project.name)}
        {#if project.error}
          <li class="rounded-lg border border-line bg-surface-raised p-4">
            <p class="font-semibold">{project.name}</p>
            <p class="mt-1 text-sm break-words text-state-crashed" role="alert">{project.error}</p>
          </li>
        {/if}
        {#each project.instances as view (view.id)}
          <li class="flex">
            <InstanceCard
              {project}
              {view}
              usage={usage.get(project.name, view.id)}
              now={usage.now}
              busy={actions.busy(project.name, view.id)}
              onaction={(action) => void actions.run(action, project.name, view.id)}
            />
          </li>
        {/each}
      {/each}
    </ul>
  {/if}
{/if}
