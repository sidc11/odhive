<script lang="ts">
  import { theme, type ThemeChoice } from '../theme.svelte'

  const LABELS: Record<ThemeChoice, string> = {
    system: 'System theme',
    light: 'Light theme',
    dark: 'Dark theme',
  }
</script>

<button
  type="button"
  class="inline-flex size-9 items-center justify-center rounded-md text-fg-muted hover:bg-surface-sunken hover:text-fg"
  aria-label="{LABELS[theme.choice]} (change theme)"
  title={LABELS[theme.choice]}
  onclick={() => theme.cycle()}
>
  <svg viewBox="0 0 24 24" class="size-5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    {#if theme.choice === 'light'}
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
    {:else if theme.choice === 'dark'}
      <path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5Z" />
    {:else}
      <rect x="3" y="4" width="18" height="12" rx="2" />
      <path d="M8 20h8M12 16v4" />
    {/if}
  </svg>
</button>
