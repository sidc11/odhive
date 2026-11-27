<script lang="ts">
  import { tick, untrack } from 'svelte'
  import { entryCount, rowsText, selectedRowIds } from './copy'
  import { buildExport, download, type ExportFormat } from './export'
  import {
    compileSearch,
    LEVEL_CHIPS,
    parseLoggers,
    rowFilter,
    rowMatcher,
    SOURCE_CHIPS,
    type LevelChip,
    type SourceChip,
  } from './filter'
  import { LogLines, type LogFeed } from './lines'
  import LogRowView from './LogRowView.svelte'
  import LogToolbar from './LogToolbar.svelte'
  import type { LogRow } from './rows'
  import { foldable, tracebackText, type FileLink } from './traceback'

  let {
    stream,
    label,
    source,
    onopen,
    showSources = true,
    idle = 'No log yet. Entries appear here when the instance runs in OdHive.',
  }: {
    stream: LogFeed
    label: string
    /** Show the Odoo/SQL source chips. */
    showSources?: boolean
    /** Shown while `stream.running` is false and nothing is there. */
    idle?: string
    /** Instance of the log, for export file names (no export without it). */
    source?: { project: string; instance: string }
    /** Opens a `file:line` of a traceback (no links without it). */
    onopen?: (link: FileLink) => void
  } = $props()

  /** Lines rendered beyond the viewport, above and below. */
  const OVERSCAN = 30
  /** Line height until the real one is measured (px). */
  const DEFAULT_LINE_HEIGHT = 20
  /** Typing pause before the logger filter and search apply (ms). */
  const TYPING_DELAY = 150

  // The parent keys this component by stream, so it never changes.
  const lines = untrack(() => new LogLines(stream))

  /** Bumped when `lines` changes (it is plain, not reactive). */
  let revision = $state(0)
  let scroller: HTMLDivElement | undefined = $state()
  let probe: HTMLSpanElement | undefined = $state()
  let lineHeight = $state(DEFAULT_LINE_HEIGHT)
  let viewport = $state(0)
  let scrollTop = $state(0)
  /** Keep the newest entry in view; paused when scrolled up. */
  let following = $state(true)
  /** Rows added while paused. */
  let unseen = $state(0)

  // Filters and search, as typed and as applied (after a typing pause).
  let levels = $state.raw(new Set<LevelChip>(LEVEL_CHIPS))
  let sources = $state.raw(new Set<SourceChip>(SOURCE_CHIPS))
  let loggers = $state('')
  let query = $state('')
  let regex = $state(false)
  let caseSensitive = $state(false)
  let appliedLoggers = $state('')
  let appliedQuery = $state('')
  let searchInput: HTMLInputElement | undefined = $state()
  /** Row id of the current search match. */
  let current: number | null = $state(null)
  let matchCount = $state(0)
  /** Rows the text selection in the log touches (by id), if any. */
  let selected: { from: number; to: number } | null = $state(null)

  const search = $derived(compileSearch(appliedQuery, { regex, caseSensitive }))
  const pattern = $derived(search.ok ? search.pattern : null)

  $effect(() => {
    const value = loggers
    const timer = setTimeout(() => (appliedLoggers = value), TYPING_DELAY)
    return () => clearTimeout(timer)
  })
  $effect(() => {
    const value = query
    if (value === '') {
      appliedQuery = ''
      return
    }
    const timer = setTimeout(() => (appliedQuery = value), TYPING_DELAY)
    return () => clearTimeout(timer)
  })

  $effect(() => {
    const filter = rowFilter(levels, parseLoggers(appliedLoggers), sources)
    untrack(() => applyFilter(filter))
  })

  $effect(() => {
    const matcher = pattern ? rowMatcher(pattern) : null
    untrack(() => {
      lines.setSearch(matcher)
      matchCount = lines.matches.length
      current = null
      revision += 1
    })
  })

  $effect(() => {
    void stream.version
    untrack(() => update())
  })

  function update(): void {
    const { added, removedLines } = lines.sync()
    if (added === 0 && removedLines === 0) return
    matchCount = lines.matches.length
    revision += 1
    if (following) {
      void tick().then(scrollToEnd)
    } else {
      unseen += added
      // Rows left at the top: keep what is on screen in place.
      if (removedLines > 0) {
        void tick().then(() => {
          if (scroller) scroller.scrollTop = Math.max(scroller.scrollTop - removedLines * lineHeight, 0)
        })
      }
    }
  }

  /** Id of the row at the top of the viewport. */
  function topRowId(): number | null {
    return lines.items[lines.layout.indexAt(scrollTop / lineHeight)]?.id ?? null
  }

  /** Shows the rows `filter` passes, keeping the top row in place when paused. */
  function applyFilter(filter: Parameters<LogLines['setFilter']>[0]): void {
    const anchor = following ? null : topRowId()
    lines.setFilter(filter)
    matchCount = lines.matches.length
    revision += 1
    void tick().then(() => {
      if (following || anchor === null || !scroller) {
        scrollToEnd()
        return
      }
      scroller.scrollTop = lines.layout.offsetOf(lines.indexOfId(anchor)) * lineHeight
      scrollTop = scroller.scrollTop
    })
  }

  /** Moves to the next (or previous) match, from the current one or the viewport. */
  function goToMatch(forward: boolean): void {
    let target: number | null
    if (current !== null) {
      target = lines.nextMatch(current, forward)
    } else {
      const index = lines.layout.indexAt((scrollTop + (forward ? 0 : viewport)) / lineHeight)
      const anchor = lines.items[index]?.id ?? 0
      target = lines.nextMatch(anchor, forward, true)
    }
    if (target === null) return
    current = target
    reveal(target)
  }

  /** Whether row `id` is unfolded (reactive through `revision`). */
  function isExpanded(id: number): boolean {
    void revision
    return lines.isExpanded(id)
  }

  /** Folds or unfolds a traceback. */
  function toggle(id: number): void {
    if (lines.setExpanded(id, !lines.isExpanded(id)) === 0) return
    revision += 1
    if (following) void tick().then(scrollToEnd)
  }

  /** Puts the traceback of `row` on the clipboard, exactly as logged. */
  async function copyTraceback(row: LogRow): Promise<boolean> {
    if (row.kind !== 'entry') return false
    try {
      await navigator.clipboard.writeText(tracebackText(row.record.message))
      return true
    } catch {
      return false
    }
  }

  /** Rows on screen (none of the overscan). */
  function onScreen(): LogRow[] {
    // A hair less than the viewport: a row starting at the bottom edge isn't on screen.
    const { start, end } = lines.layout.range(scrollTop / lineHeight, Math.max(viewport / lineHeight - 0.01, 0))
    return lines.items.slice(start, end)
  }

  /** Copies whole entries as log text; resolves to how many, or null if it failed. */
  async function copyRows(rows: LogRow[]): Promise<number | null> {
    try {
      await navigator.clipboard.writeText(rowsText(rows))
      return entryCount(rows)
    } catch {
      return null
    }
  }

  function copyVisible(): Promise<number | null> {
    return copyRows(onScreen())
  }

  function copySelection(): Promise<number | null> {
    return copyRows(selected ? lines.between(selected.from, selected.to) : [])
  }

  /** Downloads the shown rows (or only the search matches); returns the entries exported, or null if it failed. */
  function exportRows(format: ExportFormat, onlyMatching: boolean): number | null {
    if (!source) return null
    const matching = onlyMatching && pattern ? new Set(lines.matches) : null
    const rows = matching ? lines.items.filter((row) => matching.has(row.id)) : lines.items
    try {
      const file = buildExport(
        {
          ...source,
          rows,
          filter: {
            levels,
            sources,
            loggers: appliedLoggers,
            search: pattern ? appliedQuery : '',
            regex,
            caseSensitive,
            onlyMatching: matching !== null,
          },
          now: new Date(),
        },
        format,
      )
      download(file)
      return file.entries
    } catch {
      return null
    }
  }

  // Track the selection in the log; selecting pauses follow, so new rows
  // don't scroll the selected ones away.
  $effect(() => {
    const onselectionchange = (): void => {
      selected = selectedRowIds(document.getSelection(), scroller)
      if (selected) following = false
    }
    document.addEventListener('selectionchange', onselectionchange)
    return () => document.removeEventListener('selectionchange', onselectionchange)
  })

  /** Scrolls row `id` to the middle of the viewport (pausing follow), unfolding it. */
  function reveal(id: number): void {
    if (!scroller) return
    following = false
    const shown = lines.items[lines.indexOfId(id)]
    if (shown && foldable(shown) && lines.setExpanded(id, true) !== 0) revision += 1
    const index = lines.indexOfId(id)
    const rowHeight = lines.layout.linesOf(index) * lineHeight
    const top = lines.layout.offsetOf(index) * lineHeight - Math.max((viewport - rowHeight) / 2, 0)
    scroller.scrollTop = Math.max(top, 0)
    scrollTop = scroller.scrollTop
    void tick().then(() => {
      // Long lines: bring the match itself into view horizontally.
      scroller?.querySelector('[data-current] mark')?.scrollIntoView?.({ block: 'nearest', inline: 'center' })
    })
  }

  /** 1-based position of the current match among all matches, 0 if none. */
  const currentPosition = $derived.by(() => {
    void revision
    if (current === null) return 0
    return lines.matches.indexOf(current) + 1
  })

  // Ctrl/Cmd+F inside the log searches the log (the browser's find can't
  // see rows that aren't rendered).
  function onkeydown(event: KeyboardEvent): void {
    if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === 'f') {
      event.preventDefault()
      searchInput?.focus()
      searchInput?.select()
    }
  }

  const range = $derived.by(() => {
    void revision
    return lines.layout.range(scrollTop / lineHeight, viewport / lineHeight, OVERSCAN)
  })
  const visible = $derived.by(() => {
    void revision
    return lines.items.slice(range.start, range.end)
  })
  const offsetTop = $derived.by(() => {
    void revision
    return lines.layout.offsetOf(range.start) * lineHeight
  })
  const totalHeight = $derived.by(() => {
    void revision
    return lines.layout.total * lineHeight
  })
  const empty = $derived.by(() => {
    void revision
    return lines.items.length === 0
  })
  const filteredOut = $derived.by(() => {
    void revision
    return lines.items.length === 0 && stream.rows.length > 0
  })

  const jumpLabel = $derived(
    unseen > 0 ? `Jump to latest (${unseen > 9999 ? '9999+' : unseen.toLocaleString()} new)` : 'Jump to latest',
  )

  function atEnd(element: HTMLElement): boolean {
    return element.scrollHeight - element.scrollTop - element.clientHeight <= Math.max(lineHeight / 2, 4)
  }

  function scrollToEnd(): void {
    if (!scroller || !following) return
    scroller.scrollTop = scroller.scrollHeight
    scrollTop = scroller.scrollTop
  }

  function onscroll(): void {
    if (!scroller) return
    scrollTop = scroller.scrollTop
    if (atEnd(scroller)) {
      following = true
      unseen = 0
    } else {
      following = false
    }
  }

  // Scrolling up pauses at once, even while new rows keep the view at the end.
  function onwheel(event: WheelEvent): void {
    if (event.deltaY < 0 && scroller && scroller.scrollTop > 0) following = false
  }

  function jumpToLatest(): void {
    following = true
    unseen = 0
    scrollToEnd()
    scroller?.focus()
  }

  // Viewport size and line height (font size and zoom can change both).
  $effect(() => {
    const element = scroller
    if (!element) return
    const measure = (): void => {
      viewport = element.clientHeight
      const height = probe?.getBoundingClientRect().height ?? 0
      if (height > 0) lineHeight = height
      if (untrack(() => following)) void tick().then(scrollToEnd)
    }
    measure()
    if (typeof ResizeObserver !== 'function') return
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    if (probe) observer.observe(probe)
    return () => observer.disconnect()
  })
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="flex min-h-0 flex-1 flex-col gap-2" {onkeydown}>
<LogToolbar
  bind:levels
  bind:sources
  bind:loggers
  bind:query
  bind:regex
  bind:caseSensitive
  bind:searchInput
  error={search.ok ? null : search.error}
  matches={matchCount}
  current={currentPosition}
  onnext={() => goToMatch(true)}
  onprev={() => goToMatch(false)}
  canCopyVisible={!empty}
  canCopySelection={selected !== null}
  oncopyvisible={copyVisible}
  oncopyselection={copySelection}
  onexport={source ? exportRows : undefined}
  {showSources}
/>
<div class="relative flex min-h-0 flex-1 flex-col overflow-hidden rounded-md border border-line bg-surface-raised">
  <!-- Focusable so the keyboard can scroll it. -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <div
    bind:this={scroller}
    class="min-h-0 flex-1 overflow-auto font-mono text-xs leading-5"
    role="region"
    aria-label={label}
    tabindex="0"
    data-testid="log-scroller"
    {onscroll}
    {onwheel}
  >
    <span bind:this={probe} class="pointer-events-none invisible absolute block select-none" aria-hidden="true">0</span>
    <div class="relative w-max min-w-full" style:height={`${totalHeight}px`}>
      <div style:transform={`translateY(${offsetTop}px)`}>
        {#each visible as row (row.id)}
          <LogRowView
            {row}
            {lineHeight}
            search={pattern}
            current={row.id === current}
            expanded={isExpanded(row.id)}
            ontoggle={toggle}
            oncopy={copyTraceback}
            {onopen}
          />
        {/each}
      </div>
    </div>
    {#if empty}
      <p class="absolute inset-0 flex items-center justify-center p-4 text-center font-sans text-sm text-fg-muted">
        {#if filteredOut}
          No entries match the level, source and logger filters.
        {:else if stream.running === null}
          Connecting to the log…
        {:else if stream.running}
          Waiting for log output…
        {:else}
          {idle}
        {/if}
      </p>
    {/if}
  </div>

  {#if !following}
    <button
      type="button"
      class="absolute right-4 bottom-4 inline-flex min-h-9 items-center gap-1.5 rounded-full bg-accent px-4 text-sm font-medium text-accent-fg shadow-lg"
      onclick={jumpToLatest}
    >
      <span aria-hidden="true">↓</span>
      <span class="tabular-nums">{jumpLabel}</span>
    </button>
  {/if}
</div>
</div>
