<script lang="ts">
  import { databaseOutcome, databaseStages, type DatabaseJob, type StageState } from './progress'

  let { job }: { job: DatabaseJob } = $props()

  const stages = $derived(databaseStages(job))
  const outcome = $derived(databaseOutcome(job))
  /** Lines of tool output shown (the job keeps more). */
  const TAIL = 40
  const tail = $derived(job.output.slice(-TAIL))

  const LABEL: Record<StageState, string> = { active: 'In progress', done: 'Done', failed: 'Failed' }
  const DOT: Record<StageState, string> = {
    active: 'border-state-starting bg-state-starting animate-pulse',
    done: 'border-state-running bg-state-running',
    failed: 'border-state-crashed bg-state-crashed',
  }
</script>

{#if stages.length === 0 && job.state === 'running'}
  <p class="text-sm text-fg-muted">Starting…</p>
{:else}
  <ol class="flex flex-col gap-3" aria-label="Steps">
    {#each stages as stage, index (index)}
      <li class="flex gap-3" data-stage={stage.id} data-state={stage.state}>
        <span class="mt-1.5 size-2.5 shrink-0 rounded-full border-2 {DOT[stage.state]}" aria-hidden="true"></span>
        <div class="min-w-0">
          <p class="text-sm font-medium">{stage.label}<span class="sr-only">: {LABEL[stage.state]}</span></p>
          {#if stage.detail}<p class="text-sm break-all text-fg-muted">{stage.detail}</p>{/if}
        </div>
      </li>
    {/each}
  </ol>
{/if}

<p
  class="mt-4 text-sm break-words {job.state === 'failed'
    ? 'text-state-crashed'
    : job.state === 'succeeded'
      ? 'font-medium'
      : 'text-fg-muted'}"
  role={job.state === 'failed' ? 'alert' : 'status'}
  data-testid="job-outcome"
>
  {outcome}
</p>

{#if tail.length > 0}
  <details class="mt-4" open={job.state === 'failed'}>
    <summary class="cursor-pointer text-sm text-fg-muted">Output</summary>
    <pre
      class="mt-2 max-h-72 overflow-auto rounded-md bg-surface-sunken p-3 text-xs leading-relaxed"
      aria-label="Output">{#if job.output_dropped > 0 || job.output.length > TAIL}…
{/if}{tail.join('\n')}</pre>
  </details>
{/if}
