<script lang="ts">
  import type { Toast, ToastKind, ToastStore } from '../toasts.svelte'

  let { toasts }: { toasts: ToastStore } = $props()

  const STRIPE: Record<ToastKind, string> = {
    info: 'border-l-state-external',
    success: 'border-l-state-running',
    warning: 'border-l-state-starting',
    error: 'border-l-state-crashed',
  }
  const LABEL: Record<ToastKind, string> = {
    info: 'Info',
    success: 'Done',
    warning: 'Warning',
    error: 'Error',
  }

  let busy = $state<number | null>(null)

  async function run(toast: Toast, index: number): Promise<void> {
    const action = toast.actions?.[index]
    if (!action || busy !== null) return
    busy = toast.id
    try {
      await action.run()
    } finally {
      busy = null
    }
  }

  function leave(event: FocusEvent, toast: Toast): void {
    const next = event.relatedTarget
    if (next instanceof Node && (event.currentTarget as HTMLElement).contains(next)) return
    toasts.resume(toast.id)
  }
</script>

<section
  aria-label="Notifications"
  class="pointer-events-none fixed inset-x-4 bottom-4 z-50 sm:left-auto sm:w-96"
>
  <ol class="flex flex-col gap-2" aria-live="polite">
    {#each toasts.items as toast (toast.id)}
      <li
        class="pointer-events-auto rounded-lg border border-l-4 border-line bg-surface-raised p-3 shadow-lg {STRIPE[toast.kind]}"
        role={toast.kind === 'error' ? 'alert' : undefined}
        data-kind={toast.kind}
        onmouseenter={() => toasts.pause(toast.id)}
        onmouseleave={() => toasts.resume(toast.id)}
        onfocusin={() => toasts.pause(toast.id)}
        onfocusout={(event) => leave(event, toast)}
      >
        <div class="flex items-start gap-2">
          <div class="min-w-0 flex-1">
            <p class="text-sm font-medium break-words">
              <span class="sr-only">{`${LABEL[toast.kind]}: `}</span>{toast.title}
            </p>
            {#if toast.body}
              <p class="mt-1 text-sm break-words whitespace-pre-line text-fg-muted">{toast.body}</p>
            {/if}
          </div>
          <button
            type="button"
            class="inline-flex size-7 shrink-0 items-center justify-center rounded-md text-fg-muted hover:bg-surface-sunken hover:text-fg"
            aria-label="Dismiss notification"
            onclick={() => toasts.dismiss(toast.id)}
          >
            <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
              <path d="M6 6l12 12M18 6 6 18" />
            </svg>
          </button>
        </div>
        {#if toast.actions && toast.actions.length > 0}
          <div class="mt-2 flex flex-wrap gap-2">
            {#each toast.actions as action, index (action.label)}
              <button
                type="button"
                class="min-h-8 rounded-md px-3 text-sm font-medium {index === 0
                  ? 'bg-accent text-accent-fg hover:opacity-90'
                  : 'border border-line hover:bg-surface-sunken'} disabled:opacity-50"
                disabled={busy !== null}
                onclick={() => run(toast, index)}
              >
                {action.label}
              </button>
            {/each}
          </div>
        {/if}
      </li>
    {/each}
  </ol>
</section>
