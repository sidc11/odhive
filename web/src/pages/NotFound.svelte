<script lang="ts">
  import { router } from '../lib/router.svelte'

  let { path }: { path: string } = $props()
</script>

<h1 class="text-2xl font-semibold">Page not found</h1>
<p class="mt-2 text-sm text-fg-muted">
  Nothing lives at <code class="rounded bg-surface-sunken px-1 break-all">{path}</code>.
</p>
<a href="/" class="mt-4 inline-block text-sm font-medium text-accent hover:underline" onclick={router.link}>
  Back to the dashboard
</a>
