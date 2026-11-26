<script lang="ts">
  import { onMount, tick, type Snippet } from 'svelte'
  import { trapTab } from '../focus'

  let {
    title,
    subtitle,
    onclose,
    children,
    footer,
    testid,
  }: {
    title: string
    subtitle?: string
    /** Escape, the backdrop and the close button call it. */
    onclose: () => void
    children: Snippet
    footer?: Snippet
    testid?: string
  } = $props()

  const id = `modal-${Math.random().toString(36).slice(2, 10)}`
  let panel: HTMLElement | undefined = $state()

  onMount(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    void tick().then(() => {
      // The first field, else the panel itself.
      const first = panel?.querySelector<HTMLElement>('[data-autofocus], input:not([type="hidden"]):not([disabled]), select, textarea')
      ;(first ?? panel)?.focus()
    })
    return () => {
      if (opener?.isConnected) opener.focus()
    }
  })

  function onkeydown(event: KeyboardEvent): void {
    if (!panel) return
    if (event.key === 'Escape') {
      event.preventDefault()
      event.stopPropagation()
      onclose()
    } else {
      trapTab(event, panel)
    }
  }

  const button =
    'inline-flex min-h-9 items-center justify-center rounded-md border border-line px-2 text-sm font-medium hover:bg-surface-sunken'
</script>

<svelte:window {onkeydown} />

<div class="fixed inset-0 z-40 flex items-start justify-center overflow-y-auto p-4 sm:items-center">
  <button
    type="button"
    class="fixed inset-0 size-full cursor-default bg-backdrop"
    aria-label="Close {title}"
    tabindex="-1"
    onclick={onclose}
  ></button>
  <div
    bind:this={panel}
    role="dialog"
    aria-modal="true"
    aria-labelledby="{id}-title"
    aria-describedby={subtitle ? `${id}-subtitle` : undefined}
    tabindex="-1"
    class="relative flex max-h-[calc(100dvh-2rem)] w-full max-w-lg flex-col rounded-lg border border-line bg-surface-raised shadow-xl focus:outline-none"
    data-testid={testid}
  >
    <header class="flex items-start gap-2 border-b border-line p-4">
      <div class="min-w-0 flex-1">
        <h2 id="{id}-title" class="text-lg font-semibold">{title}</h2>
        {#if subtitle}<p id="{id}-subtitle" class="truncate text-sm text-fg-muted">{subtitle}</p>{/if}
      </div>
      <button type="button" class={button} aria-label="Close dialog" onclick={onclose}>
        <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
          ><path d="M6 6l12 12M18 6L6 18" /></svg
        >
      </button>
    </header>
    <div class="min-h-0 flex-1 overflow-y-auto p-4">
      {@render children()}
    </div>
    {#if footer}
      <footer class="flex flex-wrap justify-end gap-2 border-t border-line p-4">
        {@render footer()}
      </footer>
    {/if}
  </div>
</div>
