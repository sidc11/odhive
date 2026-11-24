<script lang="ts">
  import { LEVEL_CHIP_LABELS, LEVEL_CHIPS, MAX_QUERY, type LevelChip } from './filter'

  let {
    levels = $bindable(),
    loggers = $bindable(),
    query = $bindable(),
    regex = $bindable(),
    caseSensitive = $bindable(),
    error,
    matches,
    current,
    searchInput = $bindable(),
    onnext,
    onprev,
  }: {
    levels: Set<LevelChip>
    loggers: string
    query: string
    regex: boolean
    caseSensitive: boolean
    /** Why the search can't run, if it can't. */
    error: string | null
    matches: number
    /** 1-based position of the current match, 0 when none is current. */
    current: number
    searchInput?: HTMLInputElement
    onnext: () => void
    onprev: () => void
  } = $props()

  const uid = $props.id()
  const errorId = `${uid}-search-error`

  const CHIP_CLASSES: Record<LevelChip, string> = {
    debug: 'text-level-debug',
    info: 'text-level-info',
    warning: 'text-level-warning',
    error: 'text-level-error',
    critical: 'font-bold text-level-error',
  }

  function toggle(chip: LevelChip): void {
    // A new set each time: the parent reacts to the assignment.
    levels = levels.has(chip) ? new Set([...levels].filter((other) => other !== chip)) : new Set([...levels, chip])
  }

  function onkeydown(event: KeyboardEvent): void {
    if (event.key === 'Enter') {
      event.preventDefault()
      if (event.shiftKey) onprev()
      else onnext()
    } else if (event.key === 'Escape' && query !== '') {
      event.preventDefault()
      query = ''
    }
  }

  const status = $derived(
    error !== null || query === ''
      ? ''
      : matches === 0
        ? 'No matches'
        : current > 0
          ? `${current.toLocaleString()} of ${matches.toLocaleString()}`
          : `${matches.toLocaleString()} ${matches === 1 ? 'match' : 'matches'}`,
  )

  const toggleClass = (on: boolean): string =>
    `inline-flex min-h-8 min-w-8 items-center justify-center rounded px-1.5 font-mono text-xs ${
      on ? 'bg-accent text-accent-fg' : 'text-fg-muted hover:bg-surface-sunken'
    }`
</script>

<div class="flex flex-wrap items-center gap-x-3 gap-y-2" data-testid="log-toolbar">
  <div class="flex flex-wrap gap-1" role="group" aria-label="Levels">
    {#each LEVEL_CHIPS as chip (chip)}
      {@const on = levels.has(chip)}
      <button
        type="button"
        class={`inline-flex min-h-8 items-center rounded-full border px-2.5 font-mono text-xs ${
          on ? `border-line bg-surface-raised ${CHIP_CLASSES[chip]}` : 'border-dashed border-line text-fg-muted line-through'
        }`}
        aria-pressed={on}
        onclick={() => toggle(chip)}
      >
        {LEVEL_CHIP_LABELS[chip]}
      </button>
    {/each}
  </div>

  <label class="flex min-w-0 flex-1 basis-40 items-center gap-1.5 text-sm">
    <span class="text-fg-muted">Logger</span>
    <input
      bind:value={loggers}
      type="text"
      class="min-h-8 w-full min-w-0 rounded border border-line bg-surface-raised px-2 font-mono text-xs"
      placeholder="odoo.addons.sale, -werkzeug"
      autocomplete="off"
      spellcheck="false"
    />
  </label>

  <div class="flex min-w-0 flex-[2] basis-64 items-center gap-1" role="search">
    <div
      class={`flex min-w-0 flex-1 items-center rounded border bg-surface-raised has-[input:focus-visible]:outline-2 has-[input:focus-visible]:outline-offset-2 has-[input:focus-visible]:outline-focus ${error ? 'border-level-error' : 'border-line'}`}
    >
      <input
        bind:this={searchInput}
        bind:value={query}
        type="search"
        class="min-h-8 w-full min-w-0 bg-transparent px-2 font-mono text-xs focus-visible:outline-none"
        placeholder="Search"
        aria-label="Search the log"
        aria-invalid={error !== null}
        aria-describedby={error ? errorId : undefined}
        maxlength={MAX_QUERY}
        autocomplete="off"
        spellcheck="false"
        {onkeydown}
      />
      <button
        type="button"
        class={toggleClass(caseSensitive)}
        aria-pressed={caseSensitive}
        aria-label="Match case"
        title="Match case"
        onclick={() => (caseSensitive = !caseSensitive)}>Aa</button
      >
      <button
        type="button"
        class={toggleClass(regex)}
        aria-pressed={regex}
        aria-label="Regular expression"
        title="Regular expression"
        onclick={() => (regex = !regex)}>.*</button
      >
    </div>
    <span class="min-w-16 text-center text-xs text-fg-muted tabular-nums" aria-live="polite" data-testid="log-search-status"
      >{status}</span
    >
    <button
      type="button"
      class="inline-flex min-h-8 min-w-8 items-center justify-center rounded text-fg-muted hover:bg-surface-sunken disabled:opacity-50"
      aria-label="Previous match"
      title="Previous match (Shift+Enter)"
      disabled={matches === 0}
      onclick={onprev}><span aria-hidden="true">↑</span></button
    >
    <button
      type="button"
      class="inline-flex min-h-8 min-w-8 items-center justify-center rounded text-fg-muted hover:bg-surface-sunken disabled:opacity-50"
      aria-label="Next match"
      title="Next match (Enter)"
      disabled={matches === 0}
      onclick={onnext}><span aria-hidden="true">↓</span></button
    >
  </div>
  {#if error}
    <p id={errorId} class="w-full text-xs text-level-error" role="alert">Invalid search: {error}</p>
  {/if}
</div>
