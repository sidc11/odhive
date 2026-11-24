<script lang="ts">
  import type { EnvStatus } from '../api'
  import { STATUS_LABELS } from '../environment.svelte'

  let { status }: { status: EnvStatus } = $props()

  const DOTS: Record<EnvStatus, string> = {
    ok: 'bg-state-running',
    info: 'bg-state-external',
    warning: 'bg-state-starting',
    error: 'bg-state-crashed',
  }
</script>

<span
  class="inline-flex shrink-0 items-center gap-1.5 rounded-full border border-line px-2 py-0.5 text-xs font-medium"
  data-status={status}
  aria-hidden="true"
>
  <span class="inline-block size-2 rounded-full {DOTS[status]}"></span>
  {STATUS_LABELS[status]}
</span>
