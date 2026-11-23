<script lang="ts">
  import type { LiveConnection } from '../live.svelte'

  let { connection }: { connection: LiveConnection } = $props()

  const offline = $derived(connection.status === 'reconnecting')
</script>

<div role="status" class="contents">
  {#if offline}
    <span
      class="inline-flex items-center gap-1.5 rounded-full border border-line px-2 py-0.5 text-xs text-fg-muted"
      title="Live updates are paused until OdHive is reachable again."
    >
      <span class="inline-block size-2 rounded-full bg-state-starting motion-safe:animate-pulse" aria-hidden="true"></span>
      Reconnecting…
    </span>
  {/if}
</div>
