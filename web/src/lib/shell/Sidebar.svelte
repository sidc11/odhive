<script lang="ts">
  import { projects } from '../projects.svelte'
  import { routeHref, router } from '../router.svelte'
  import { instanceState, upCount } from '../status'
  import StatusDot from './StatusDot.svelte'

  /** Called after a link is followed (closes the drawer on small screens). */
  let { onnavigate }: { onnavigate?: () => void } = $props()

  const route = $derived(router.route)

  function follow(event: MouseEvent): void {
    router.link(event)
    onnavigate?.()
  }

  const item =
    'flex min-h-9 items-center gap-2 rounded-md px-2 text-sm hover:bg-surface-sunken aria-[current=page]:bg-accent-soft aria-[current=page]:font-medium aria-[current=page]:text-fg'
</script>

<div class="flex h-full flex-col gap-4 overflow-y-auto p-3">
  <a href="/" class="flex items-center gap-2 px-2 pt-1 text-lg font-bold text-fg" onclick={follow}>
    <img src="/favicon.svg" alt="" class="size-7" />
    <span>OdHive</span>
  </a>

  <nav aria-label="Main" class="flex flex-col gap-4">
    <a href="/" class={item} aria-current={route.name === 'dashboard' ? 'page' : undefined} onclick={follow}>
      Dashboard
    </a>

    <section aria-labelledby="nav-projects" class="flex flex-col gap-1">
      <h2 id="nav-projects" class="px-2 text-xs font-semibold tracking-wide text-fg-muted uppercase">
        Projects
      </h2>

      {#if projects.state.status === 'loading'}
        <p class="px-2 text-sm text-fg-muted">Loading…</p>
      {:else if projects.state.status === 'error'}
        <p class="px-2 text-sm text-state-crashed" role="alert">{projects.state.message}</p>
      {:else if projects.state.projects.length === 0}
        <p class="px-2 text-sm text-fg-muted">No projects yet.</p>
      {:else}
        <ul class="flex flex-col gap-0.5">
          {#each projects.state.projects as project (project.name)}
            {@const up = upCount(project)}
            <li>
              <a
                href={routeHref({ name: 'project', project: project.name })}
                class="{item} text-fg"
                aria-current={route.name === 'project' && route.project === project.name ? 'page' : undefined}
                onclick={follow}
              >
                <span class="min-w-0 flex-1 truncate" title={project.name}>{project.name}</span>
                {#if project.instances.length > 0}
                  <span class="text-xs text-fg-muted tabular-nums" title="{up} of {project.instances.length} running">
                    {up}/{project.instances.length}
                  </span>
                {/if}
              </a>
              {#if project.instances.length > 0}
                <ul class="ml-3 flex flex-col gap-0.5 border-l border-line pl-2">
                  {#each project.instances as instance (instance.id)}
                    <li>
                      <a
                        href={routeHref({ name: 'instance', project: project.name, instance: instance.id })}
                        class="{item} text-fg-muted"
                        aria-current={route.name === 'instance' &&
                        route.project === project.name &&
                        route.instance === instance.id
                          ? 'page'
                          : undefined}
                        onclick={follow}
                      >
                        <StatusDot state={instanceState(instance)} />
                        <span class="min-w-0 flex-1 truncate" title={instance.id}>{instance.id}</span>
                      </a>
                    </li>
                  {/each}
                </ul>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  </nav>
</div>
