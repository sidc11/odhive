<script lang="ts">
  import type { EnvCheck } from '../api'
  import { CHECK_LABELS, CHECK_PURPOSES, STATUS_LABELS } from '../environment.svelte'
  import EnvStatusBadge from './EnvStatusBadge.svelte'

  let { check, manager }: { check: EnvCheck; manager: string | null } = $props()

  /** Index of the command just copied, shown as “Copied” for a moment. */
  let copied = $state<number | null>(null)
  let copyFailed = $state(false)
  let timer: ReturnType<typeof setTimeout> | undefined

  async function copy(command: string, index: number): Promise<void> {
    clearTimeout(timer)
    try {
      await navigator.clipboard.writeText(command)
      copied = index
      copyFailed = false
    } catch {
      copied = null
      copyFailed = true
    }
    timer = setTimeout(() => {
      copied = null
      copyFailed = false
    }, 2000)
  }

  $effect(() => () => clearTimeout(timer))

  const headingId = $derived(`env-${check.id}`)
</script>

<article
  class="flex flex-col gap-3 rounded-lg border border-line bg-surface-raised p-4"
  aria-labelledby={headingId}
  data-status={check.status}
>
  <header class="flex flex-wrap items-start gap-x-3 gap-y-1">
    <div class="min-w-0 flex-1">
      <h2 id={headingId} class="flex flex-wrap items-baseline gap-x-2 font-semibold">
        <span>{CHECK_LABELS[check.id]}</span>
        {#if check.version}
          <span class="text-sm font-normal text-fg-muted tabular-nums">{check.version}</span>
        {/if}
        <span class="sr-only">: {STATUS_LABELS[check.status]}</span>
      </h2>
      <p class="text-sm text-fg-muted">{CHECK_PURPOSES[check.id]}</p>
    </div>
    <EnvStatusBadge status={check.status} />
  </header>

  <div class="flex flex-col gap-1 text-sm">
    <p>{check.summary}</p>
    {#if check.location}
      <p class="font-mono text-xs break-all text-fg-muted">{check.location}</p>
    {/if}
    {#if check.details.length > 0}
      <ul class="mt-1 flex flex-col gap-0.5 text-fg-muted">
        {#each check.details as detail, i (i)}
          <li class="break-words">{detail}</li>
        {/each}
      </ul>
    {/if}
  </div>

  {#if check.fix}
    <div class="flex flex-col gap-2 rounded-md border border-line bg-surface p-3 text-sm">
      <p><span class="font-medium">How to fix:</span> {check.fix.text}</p>
      {#if check.fix.commands.length > 0}
        <ol class="flex flex-col gap-1.5" aria-label="Commands{manager ? ` for ${manager}` : ''}">
          {#each check.fix.commands as command, i (i)}
            <li class="flex items-stretch gap-2">
              <code
                class="min-w-0 flex-1 overflow-x-auto rounded border border-line bg-surface-sunken px-2 py-1.5 font-mono text-xs whitespace-pre"
                >{command}</code
              >
              <button
                type="button"
                class="inline-flex min-h-9 shrink-0 items-center rounded-md border border-line px-2.5 text-xs font-medium hover:bg-surface-sunken"
                onclick={() => copy(command, i)}
                aria-label="Copy command: {command}"
              >
                {copied === i ? 'Copied' : 'Copy'}
              </button>
            </li>
          {/each}
        </ol>
        {#if copyFailed}
          <p class="text-xs text-state-crashed" role="alert">Could not copy; select the command instead.</p>
        {/if}
      {/if}
      {#if check.fix.url}
        <a href={check.fix.url} target="_blank" rel="noopener noreferrer" class="w-fit text-accent underline underline-offset-2"
          >Read more<span class="sr-only"> about {CHECK_LABELS[check.id]} (opens in a new tab)</span></a
        >
      {/if}
    </div>
  {/if}
</article>
