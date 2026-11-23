<script lang="ts">
  import type { Job } from '../api'
  import { setupStages, type StageState } from './progress'

  let { job, install }: { job: Job; install: boolean } = $props()

  const stages = $derived(setupStages(job, install))
  /** Lines of tool output shown (the job keeps more). */
  const TAIL = 40
  const tail = $derived(job.output.slice(-TAIL))

  const LABEL: Record<StageState, string> = {
    pending: 'Waiting',
    active: 'In progress',
    done: 'Done',
    failed: 'Failed',
    skipped: 'Skipped',
  }
  const DOT: Record<StageState, string> = {
    pending: 'border-line bg-surface',
    active: 'border-state-starting bg-state-starting animate-pulse',
    done: 'border-state-running bg-state-running',
    failed: 'border-state-crashed bg-state-crashed',
    skipped: 'border-line bg-surface-sunken',
  }
</script>

<ol class="flex flex-col gap-3" aria-label="Setup steps">
  {#each stages as stage (stage.id)}
    <li class="flex gap-3" data-stage={stage.id} data-state={stage.state}>
      <span class="mt-1.5 size-2.5 shrink-0 rounded-full border-2 {DOT[stage.state]}" aria-hidden="true"></span>
      <div class="min-w-0">
        <p class="text-sm font-medium">
          {stage.label}<span class="sr-only">: {LABEL[stage.state]}</span>
          {#if stage.state === 'skipped'}<span class="text-fg-muted"> · skipped</span>{/if}
        </p>
        {#if stage.detail}<p class="text-sm break-all text-fg-muted">{stage.detail}</p>{/if}
        {#if stage.warning}<p class="text-sm text-state-starting">{stage.warning}</p>{/if}
      </div>
    </li>
  {/each}
</ol>

{#if tail.length > 0}
  <details class="mt-4" open={job.state === 'failed'}>
    <summary class="cursor-pointer text-sm text-fg-muted">Output</summary>
    <pre
      class="mt-2 max-h-72 overflow-auto rounded-md bg-surface-sunken p-3 text-xs leading-relaxed"
      aria-label="Setup output">{#if job.output_dropped > 0 || job.output.length > TAIL}…
{/if}{tail.join('\n')}</pre>
  </details>
{/if}
