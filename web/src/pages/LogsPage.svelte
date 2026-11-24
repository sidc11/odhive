<script lang="ts">
  import LogPanel from '../lib/logs/LogPanel.svelte'
  import { gridClass, keyedPanes, paneClass, withoutPane, withPane } from '../lib/logs/split'
  import { MAX_PANES, routeHref, router, type LogPane } from '../lib/router.svelte'
  import StatusDot from '../lib/shell/StatusDot.svelte'
  import { instanceState, STATE_LABELS } from '../lib/status'
  import { projects } from '../lib/stores'

  let { panes }: { panes: LogPane[] } = $props()

  const keyed = $derived(keyedPanes(panes))
  const full = $derived(panes.length >= MAX_PANES)
  const choices = $derived(
    projects.state.status === 'ready'
      ? projects.state.projects.filter((project) => project.instances.length > 0)
      : [],
  )
  /** `project/instance` picked in the add control. */
  let picked = $state('')

  const findView = (pane: LogPane) =>
    projects.find(pane.project)?.instances.find((candidate) => candidate.id === pane.instance)

  function add(event: SubmitEvent): void {
    event.preventDefault()
    const [project, instance] = picked.split('/')
    if (!project || !instance || full) return
    router.navigate(withPane(panes, { project, instance }))
    picked = ''
  }

  function close(index: number): void {
    router.navigate(withoutPane(panes, index))
  }
</script>

<!-- From lg the logs share the viewport below the top bar (20rem rows at least); below, they stack. -->
<div class="flex flex-col lg:min-h-[calc(100dvh-8.5rem)]">
  <div class="flex flex-wrap items-center gap-3">
    <h1 class="min-w-0 flex-1 truncate text-2xl font-semibold">Logs</h1>
    <form class="flex flex-wrap items-center gap-2" onsubmit={add}>
      <label for="add-log" class="sr-only">Instance to add</label>
      <select
        id="add-log"
        bind:value={picked}
        disabled={full || choices.length === 0}
        class="min-h-9 max-w-56 rounded-md border border-line bg-surface-raised px-2 text-sm disabled:opacity-60"
      >
        <option value="">{full ? `${MAX_PANES} logs at most` : 'Choose an instance…'}</option>
        {#each choices as project (project.name)}
          <optgroup label={project.name}>
            {#each project.instances as view (view.id)}
              <option value={`${project.name}/${view.id}`}>{project.name}/{view.id}</option>
            {/each}
          </optgroup>
        {/each}
      </select>
      <button
        type="submit"
        disabled={full || picked === ''}
        class="inline-flex min-h-9 items-center rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:opacity-60 disabled:hover:bg-transparent"
        >Add log</button
      >
    </form>
  </div>

  {#if panes.length === 0}
    <p class="mt-4 text-sm text-fg-muted">
      Choose up to {MAX_PANES} instances to follow their logs side by side.
    </p>
  {:else}
    <div class="mt-4 grid flex-1 grid-cols-1 gap-3 {gridClass(panes.length)}">
      {#each keyed as pane, index (pane.key)}
        {@const name = `${pane.project}/${pane.instance}`}
        {@const view = findView(pane)}
        <section
          class="flex h-[75dvh] min-h-80 min-w-0 flex-col gap-2 lg:h-auto lg:min-h-0 {paneClass(panes.length, index)}"
          aria-labelledby="pane-{index}"
        >
          <div class="flex items-center gap-2">
            {#if view}
              {@const state = instanceState(view)}
              <StatusDot {state} />
              <span class="sr-only">{STATE_LABELS[state]}:</span>
            {/if}
            <h2 id="pane-{index}" class="min-w-0 flex-1 truncate text-sm font-semibold">
              <a
                href={routeHref({ name: 'instance', project: pane.project, instance: pane.instance })}
                class="hover:underline"
                onclick={router.link}>{name}</a
              >
            </h2>
            <button
              type="button"
              class="inline-flex size-8 shrink-0 items-center justify-center rounded-md text-fg-muted hover:bg-surface-sunken hover:text-fg"
              title="Close"
              onclick={() => close(index)}
            >
              <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
                ><path d="M6 6l12 12M18 6L6 18" /></svg
              >
              <span class="sr-only">Close log of {name}</span>
            </button>
          </div>
          {#if view}
            <div class="flex min-h-0 flex-1 flex-col">
              <LogPanel project={pane.project} instance={pane.instance} />
            </div>
          {:else if projects.state.status === 'ready'}
            <p class="text-sm text-fg-muted">No instance “{pane.instance}” in project “{pane.project}”.</p>
          {:else}
            <p class="text-sm text-fg-muted">Loading…</p>
          {/if}
        </section>
      {/each}
    </div>
  {/if}
</div>
