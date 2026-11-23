<script lang="ts">
  import { projects } from '../lib/stores'
  import { routeHref, router } from '../lib/router.svelte'
  import { instanceState, STATE_LABELS } from '../lib/status'
  import StatusDot from '../lib/shell/StatusDot.svelte'

  let { name }: { name: string } = $props()

  const project = $derived(projects.find(name))
</script>

<h1 class="truncate text-2xl font-semibold">{name}</h1>

{#if project}
  {#if project.error}
    <p class="mt-2 text-sm text-state-crashed" role="alert">{project.error}</p>
  {/if}
  <p class="mt-2 text-sm text-fg-muted">
    {project.odoo_version ? `Odoo ${project.odoo_version}` : 'Odoo version unknown'}
  </p>
  <h2 class="mt-6 text-lg font-semibold">Instances</h2>
  {#if project.instances.length === 0}
    <p class="mt-2 text-sm text-fg-muted">No instances.</p>
  {:else}
    <ul class="mt-2 flex flex-col divide-y divide-line rounded-lg border border-line bg-surface-raised">
      {#each project.instances as instance (instance.id)}
        {@const state = instanceState(instance)}
        <li>
          <a
            href={routeHref({ name: 'instance', project: name, instance: instance.id })}
            class="flex items-center gap-3 px-4 py-3 hover:bg-surface-sunken"
            onclick={router.link}
          >
            <StatusDot {state} />
            <span class="min-w-0 flex-1 truncate font-medium">{instance.id}</span>
            <span class="text-sm text-fg-muted" aria-hidden="true">{STATE_LABELS[state]}</span>
          </a>
        </li>
      {/each}
    </ul>
  {/if}
{:else if projects.state.status === 'ready'}
  <p class="mt-2 text-sm text-fg-muted">No project named “{name}”.</p>
{/if}
