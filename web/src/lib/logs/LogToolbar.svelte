<script lang="ts">
  import { EXPORT_FORMATS, type ExportFormat } from './export'
  import {
    LEVEL_CHIP_LABELS,
    LEVEL_CHIPS,
    MAX_QUERY,
    SOURCE_CHIP_LABELS,
    SOURCE_CHIPS,
    type LevelChip,
    type SourceChip,
  } from './filter'

  let {
    levels = $bindable(),
    sources = $bindable(),
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
    canCopyVisible = false,
    canCopySelection = false,
    oncopyvisible,
    oncopyselection,
    onexport,
  }: {
    levels: Set<LevelChip>
    sources: Set<SourceChip>
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
    /** Whether there are rows on screen to copy. */
    canCopyVisible?: boolean
    /** Whether text in the log is selected. */
    canCopySelection?: boolean
    /** Copy actions; resolve to the entries copied, or null if it failed. */
    oncopyvisible?: () => Promise<number | null>
    oncopyselection?: () => Promise<number | null>
    /** Downloads the shown entries (only search matches if asked); returns how many, or null if it failed. */
    onexport?: (format: ExportFormat, onlyMatching: boolean) => number | null
  } = $props()

  /** How long a copy result shows (ms). */
  const COPIED_MS = 1500

  /** Result of the last copy, shown in place of its button's label for a moment. */
  let copied: { which: 'visible' | 'selection'; text: string } | null = $state(null)
  let copiedTimer: ReturnType<typeof setTimeout> | undefined

  async function copy(which: 'visible' | 'selection'): Promise<void> {
    const action = which === 'visible' ? oncopyvisible : oncopyselection
    if (!action) return
    const count = await action()
    const text =
      count === null ? 'Copy failed' : `Copied ${count.toLocaleString()} ${count === 1 ? 'entry' : 'entries'}`
    copied = { which, text }
    clearTimeout(copiedTimer)
    copiedTimer = setTimeout(() => (copied = null), COPIED_MS)
  }

  $effect(() => () => clearTimeout(copiedTimer))

  // A click on a copy button keeps the text selection (and focus) where it is.
  function keepSelection(event: MouseEvent): void {
    event.preventDefault()
  }

  const copyLabel = (which: 'visible' | 'selection', label: string): string =>
    copied?.which === which ? copied.text : label

  const uid = $props.id()
  const errorId = `${uid}-search-error`
  const exportId = `${uid}-export`

  const EXPORT_TITLES: Record<ExportFormat, string> = {
    log: 'Odoo log lines, as in the log file (UTC)',
    txt: 'Readable text in local time, with the filters used',
    json: 'Log records as JSON, with the filters used',
  }

  let exportOpen = $state(false)
  let onlyMatching = $state(false)
  let exportButton: HTMLButtonElement | undefined = $state()
  let exportBox: HTMLDivElement | undefined = $state()
  /** Result of the last export, shown in place of the Export label for a moment. */
  let exported: string | null = $state(null)
  let exportedTimer: ReturnType<typeof setTimeout> | undefined

  const searching = $derived(query !== '' && error === null)

  function runExport(format: ExportFormat): void {
    if (!onexport) return
    const count = onexport(format, searching && onlyMatching)
    exported =
      count === null ? 'Export failed' : `Exported ${count.toLocaleString()} ${count === 1 ? 'entry' : 'entries'}`
    clearTimeout(exportedTimer)
    exportedTimer = setTimeout(() => (exported = null), COPIED_MS)
    closeExport(true)
  }

  function closeExport(refocus: boolean): void {
    exportOpen = false
    if (refocus) exportButton?.focus()
  }

  $effect(() => () => clearTimeout(exportedTimer))

  // A press outside the export box closes it; Escape closes it and returns focus.
  $effect(() => {
    if (!exportOpen) return
    const onpointerdown = (event: PointerEvent): void => {
      if (event.target instanceof Node && exportBox?.contains(event.target)) return
      closeExport(false)
    }
    document.addEventListener('pointerdown', onpointerdown)
    return () => document.removeEventListener('pointerdown', onpointerdown)
  })

  function onexportkeydown(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      event.preventDefault()
      event.stopPropagation()
      closeExport(true)
    }
  }

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

  function toggleSource(chip: SourceChip): void {
    sources = sources.has(chip) ? new Set([...sources].filter((other) => other !== chip)) : new Set([...sources, chip])
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

  <div class="flex flex-wrap gap-1" role="group" aria-label="Sources">
    {#each SOURCE_CHIPS as chip (chip)}
      {@const on = sources.has(chip)}
      <button
        type="button"
        class={`inline-flex min-h-8 items-center rounded-full border px-2.5 text-xs ${
          on ? 'border-line bg-surface-raised text-fg' : 'border-dashed border-line text-fg-muted line-through'
        }`}
        aria-pressed={on}
        title={chip === 'sql' ? 'SQL queries: turn on “Log SQL queries” in the instance settings' : undefined}
        onclick={() => toggleSource(chip)}
      >
        {SOURCE_CHIP_LABELS[chip]}
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
  <div class="flex items-center gap-1" role="group" aria-label="Copy and export">
    <button
      type="button"
      class="inline-flex min-h-8 items-center rounded border border-line px-2 text-xs text-fg-muted hover:bg-surface-sunken hover:text-fg disabled:opacity-50"
      title="Copy the entries on screen as log text"
      aria-live="polite"
      onmousedown={keepSelection}
      disabled={!canCopyVisible}
      onclick={() => void copy('visible')}>{copyLabel('visible', 'Copy visible')}</button
    >
    <button
      type="button"
      class="inline-flex min-h-8 items-center rounded border border-line px-2 text-xs text-fg-muted hover:bg-surface-sunken hover:text-fg disabled:opacity-50"
      title="Copy the whole entries the selection touches as log text (tracebacks included)"
      aria-live="polite"
      onmousedown={keepSelection}
      disabled={!canCopySelection && copied?.which !== 'selection'}
      onclick={() => void copy('selection')}>{copyLabel('selection', 'Copy selection')}</button
    >
    {#if onexport}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div class="relative" bind:this={exportBox} onkeydown={onexportkeydown}>
        <button
          bind:this={exportButton}
          type="button"
          class="inline-flex min-h-8 items-center gap-1 rounded border border-line px-2 text-xs text-fg-muted hover:bg-surface-sunken hover:text-fg disabled:opacity-50"
          title="Download the entries the filters show"
          aria-expanded={exportOpen}
          aria-controls={exportId}
          aria-live="polite"
          disabled={!canCopyVisible && exported === null}
          onclick={() => (exportOpen = !exportOpen)}
          >{exported ?? 'Export'}<span aria-hidden="true">▾</span></button
        >
        {#if exportOpen}
          <div
            id={exportId}
            class="absolute top-full right-0 z-20 mt-1 flex w-60 flex-col gap-1 rounded-md border border-line bg-surface-raised p-2 text-xs shadow-lg"
            role="group"
            aria-label="Export options"
          >
            <label class={`flex min-h-8 items-center gap-2 px-1 ${searching ? '' : 'text-fg-muted'}`}>
              <input type="checkbox" bind:checked={onlyMatching} disabled={!searching} />
              Only search matches
            </label>
            {#each EXPORT_FORMATS as format (format)}
              <button
                type="button"
                class="flex min-h-8 flex-col items-start rounded px-2 py-1 text-left hover:bg-surface-sunken"
                onclick={() => runExport(format)}
                ><span class="font-mono text-fg">Download .{format}</span><span class="text-fg-muted"
                  >{EXPORT_TITLES[format]}</span
                ></button
              >
            {/each}
          </div>
        {/if}
      </div>
    {/if}
  </div>
  {#if error}
    <p id={errorId} class="w-full text-xs text-level-error" role="alert">Invalid search: {error}</p>
  {/if}
</div>
