<script lang="ts">
  import { STATE_LABELS, type InstanceState } from '../status'

  let { state }: { state: InstanceState } = $props()

  const COLORS: Record<InstanceState, string> = {
    starting: 'bg-state-starting motion-safe:animate-pulse',
    running: 'bg-state-running',
    stopping: 'bg-state-starting motion-safe:animate-pulse',
    stopped: 'bg-state-stopped',
    crashed: 'bg-state-crashed',
    failed_to_start: 'bg-state-crashed',
    external: 'bg-state-external',
  }
</script>

<span
  class="inline-block size-2 shrink-0 rounded-full {COLORS[state]}"
  data-state={state}
  title={STATE_LABELS[state]}
  aria-hidden="true"
></span>
<span class="sr-only">{STATE_LABELS[state]}</span>
