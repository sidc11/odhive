<script lang="ts">
  import { tick, untrack } from 'svelte'
  import { LogLines } from './lines'
  import LogRowView from './LogRowView.svelte'
  import type { LogStream } from './stream.svelte'

  let { stream, label }: { stream: LogStream; label: string } = $props()

  /** Lines rendered beyond the viewport, above and below. */
  const OVERSCAN = 30
  /** Line height until the real one is measured (px). */
  const DEFAULT_LINE_HEIGHT = 20

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

  $effect(() => {
    void stream.version
    untrack(() => update())
  })

  function update(): void {
    const { added, removedLines } = lines.sync()
    if (added === 0 && removedLines === 0) return
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
    <span bind:this={probe} class="pointer-events-none invisible absolute block" aria-hidden="true">0</span>
    <div class="relative w-max min-w-full" style:height={`${totalHeight}px`}>
      <div style:transform={`translateY(${offsetTop}px)`}>
        {#each visible as row (row.id)}
          <LogRowView {row} {lineHeight} />
        {/each}
      </div>
    </div>
    {#if empty}
      <p class="absolute inset-0 flex items-center justify-center p-4 text-center font-sans text-sm text-fg-muted">
        {#if stream.running === null}
          Connecting to the log…
        {:else if stream.running}
          Waiting for log output…
        {:else}
          No log yet. Entries appear here when the instance runs in OdHive.
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
