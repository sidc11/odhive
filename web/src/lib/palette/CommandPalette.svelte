<script lang="ts">
  import { tick } from 'svelte'
  import type { ProjectsStore } from '../projects.svelte'
  import { router } from '../router.svelte'
  import StatusDot from '../shell/StatusDot.svelte'
  import { STATE_LABELS } from '../status'
  import {
    actions as defaultActions,
    palette as defaultPalette,
    projects as defaultProjects,
  } from '../stores'
  import { buildCommands, highlight, search, type CommandDeps, type Match } from './commands'
  import { isPaletteShortcut, type PaletteStore } from './palette.svelte'

  let {
    palette = defaultPalette,
    projects = defaultProjects,
    deps = {
      navigate: (href) => router.navigate(href),
      act: (action, project, instance) => void defaultActions.run(action, project, instance),
    },
  }: { palette?: PaletteStore; projects?: Pick<ProjectsStore, 'state'>; deps?: CommandDeps } = $props()

  let query = $state('')
  let active = $state(0)
  let input: HTMLInputElement | undefined = $state()
  let list: HTMLUListElement | undefined = $state()

  const commands = $derived(buildCommands(projects.state, deps))
  const results = $derived(search(commands, query))
  const activeIndex = $derived(results.length === 0 ? -1 : Math.min(active, results.length - 1))

  /** Options are rendered under group headings, in result order. */
  const sections = $derived.by(() => {
    const out: { group: string; items: { match: Match; index: number }[] }[] = []
    results.forEach((match, index) => {
      // With a query the best match leads, so headings would split it up.
      const group = query.trim() === '' ? match.command.group : 'Results'
      const last = out[out.length - 1]
      if (last && last.group === group) last.items.push({ match, index })
      else out.push({ group, items: [{ match, index }] })
    })
    return out
  })

  const optionId = (index: number): string => `palette-option-${index}`

  /** The title first, then state and hint (the dot comes first visually). */
  function optionLabel(match: Match): string {
    const { title, state, hint } = match.command
    return [title, state ? STATE_LABELS[state] : null, hint].filter(Boolean).join(', ')
  }

  // Fresh query each time it opens; focus the field once it is drawn.
  $effect(() => {
    if (!palette.open) return
    query = ''
    active = 0
    void tick().then(() => input?.focus())
  })

  $effect(() => {
    void query
    active = 0
  })

  // Keep the highlighted option in view while moving with the keyboard.
  $effect(() => {
    if (!palette.open || !list) return
    const element = list.querySelector<HTMLElement>(`#${optionId(activeIndex)}`)
    element?.scrollIntoView?.({ block: 'nearest' })
  })

  function run(match: Match | undefined): void {
    if (!match) return
    palette.hide()
    match.command.run()
  }

  function move(by: number): void {
    if (results.length === 0) return
    active = (active + by + results.length) % results.length
  }

  function onkeydown(event: KeyboardEvent): void {
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault()
        move(1)
        break
      case 'ArrowUp':
        event.preventDefault()
        move(-1)
        break
      case 'PageDown':
        event.preventDefault()
        active = Math.min(active + 8, results.length - 1)
        break
      case 'PageUp':
        event.preventDefault()
        active = Math.max(active - 8, 0)
        break
      case 'Enter':
        event.preventDefault()
        run(results[activeIndex])
        break
      case 'Escape':
        event.preventDefault()
        palette.hide()
        break
      case 'Tab':
        // The field is the only stop: the list is driven by arrows.
        event.preventDefault()
        break
    }
  }

  function onglobalkey(event: KeyboardEvent): void {
    if (!isPaletteShortcut(event)) return
    // Also Firefox's own Ctrl+K (web search).
    event.preventDefault()
    palette.toggle()
  }
</script>

<svelte:window onkeydown={onglobalkey} />

{#if palette.open}
  <div class="fixed inset-0 z-50 flex items-start justify-center px-4 pt-[12vh]">
    <button
      type="button"
      class="absolute inset-0 size-full cursor-default bg-backdrop"
      aria-label="Close command palette"
      tabindex="-1"
      onclick={() => palette.hide()}
    ></button>
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      class="relative flex max-h-[70vh] w-full max-w-xl flex-col overflow-hidden rounded-lg border border-line bg-surface-raised shadow-2xl"
    >
      <div class="flex items-center gap-2 border-b border-line px-3">
        <svg viewBox="0 0 24 24" class="size-4 shrink-0 text-fg-muted" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
          ><circle cx="11" cy="11" r="6.5" /><path d="m16 16 4 4" /></svg
        >
        <input
          bind:this={input}
          bind:value={query}
          {onkeydown}
          type="text"
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-autocomplete="list"
          aria-activedescendant={activeIndex === -1 ? undefined : optionId(activeIndex)}
          aria-label="Search commands"
          placeholder="Search projects and actions…"
          autocomplete="off"
          spellcheck="false"
          class="h-12 min-w-0 flex-1 bg-transparent text-base text-fg placeholder:text-fg-muted focus:outline-none"
        />
        <kbd class="hidden rounded border border-line px-1.5 py-0.5 text-xs text-fg-muted sm:inline">Esc</kbd>
      </div>

      <ul bind:this={list} id="palette-list" role="listbox" aria-label="Commands" class="min-h-0 flex-1 overflow-y-auto p-1.5">
        {#each sections as section (section.group)}
          <li role="presentation">
            <div class="px-2 pt-2 pb-1 text-xs font-semibold tracking-wide text-fg-muted uppercase" aria-hidden="true">
              {section.group}
            </div>
            <ul role="group" aria-label={section.group}>
              {#each section.items as { match, index } (match.command.id)}
                <!-- Keys go to the combobox (aria-activedescendant); options take clicks only. -->
                <!-- svelte-ignore a11y_click_events_have_key_events -->
                <li
                  id={optionId(index)}
                  role="option"
                  aria-selected={index === activeIndex}
                  aria-label={optionLabel(match)}
                  class="flex min-h-10 cursor-pointer items-center gap-2.5 rounded-md px-2 py-1.5 text-sm aria-selected:bg-accent-soft"
                  onclick={() => run(match)}
                  onmousemove={() => (active = index)}
                >
                  {#if match.command.state}
                    <StatusDot state={match.command.state} />
                  {:else}
                    <span class="inline-block size-2 shrink-0" aria-hidden="true"></span>
                  {/if}
                  <span class="min-w-0 flex-1 truncate">
                    {#each highlight(match.command.title, match.positions) as part, partIndex (partIndex)}
                      {#if part.mark}<mark class="bg-transparent font-semibold text-fg underline decoration-accent decoration-2 underline-offset-2">{part.text}</mark>{:else}{part.text}{/if}
                    {/each}
                  </span>
                  {#if match.command.hint}
                    <span class="hidden max-w-[45%] shrink-0 truncate text-xs text-fg-muted sm:inline">{match.command.hint}</span>
                  {/if}
                </li>
              {/each}
            </ul>
          </li>
        {/each}
      </ul>

      {#if results.length === 0}
        <p class="px-4 pb-4 text-sm text-fg-muted">No commands match “{query}”.</p>
      {/if}
      <p class="sr-only" role="status">
        {results.length === 0 ? 'No results' : `${results.length} ${results.length === 1 ? 'result' : 'results'}`}
      </p>
      <p class="hidden gap-3 border-t border-line px-3 py-2 text-xs text-fg-muted sm:flex" aria-hidden="true">
        <span><kbd class="font-sans">↑↓</kbd> move</span>
        <span><kbd class="font-sans">Enter</kbd> run</span>
        <span><kbd class="font-sans">Esc</kbd> close</span>
      </p>
    </div>
  </div>
{/if}
