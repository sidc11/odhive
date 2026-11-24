<script lang="ts">
  import { onMount } from 'svelte'
  import CheckCard from '../lib/environment/CheckCard.svelte'
  import { worstStatus, type EnvironmentStore } from '../lib/environment.svelte'
  import { environment as defaultEnvironment } from '../lib/stores'

  let { environment = defaultEnvironment }: { environment?: EnvironmentStore } = $props()

  const state = $derived(environment.state)
  const report = $derived(environment.report)
  const checking = $derived(state.status === 'loading')
  const worst = $derived(report ? worstStatus(report.checks) : null)
  const problems = $derived(report ? report.checks.filter((c) => c.status === 'error').length : 0)

  const verdict = $derived.by(() => {
    switch (worst) {
      case 'error':
        return `${problems} ${problems === 1 ? 'problem needs' : 'problems need'} fixing before Odoo can run.`
      case 'warning':
        return 'Odoo can run, with the limits noted below.'
      case 'ok':
      case 'info':
        return 'Everything Odoo needs is here.'
      default:
        return null
    }
  })

  const checkedAt = $derived(
    state.status === 'ready'
      ? new Date(state.checkedAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })
      : null,
  )

  // Always fresh when the page opens.
  onMount(() => {
    void environment.check()
  })

  const VERDICT_TONE = {
    error: 'border-state-crashed',
    warning: 'border-state-starting',
    ok: 'border-state-running',
    info: 'border-state-running',
  } as const
</script>

<div class="flex flex-wrap items-start gap-3">
  <div class="min-w-0 flex-1">
    <h1 class="text-2xl font-semibold">Environment</h1>
    <p class="mt-1 text-sm text-fg-muted">What Odoo development needs on this machine.</p>
  </div>
  <button
    type="button"
    class="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50"
    onclick={() => environment.check()}
    disabled={checking}
  >
    {checking ? 'Checking…' : 'Check again'}
  </button>
</div>

<div class="sr-only" role="status">
  {#if checking}Checking the environment…{:else if verdict}{verdict}{/if}
</div>

{#if state.status === 'error'}
  <p class="mt-4 text-sm text-state-crashed" role="alert">Could not run the check: {state.message}</p>
{/if}

{#if report && worst}
  <div class="mt-4 rounded-lg border-l-4 bg-surface-raised px-4 py-3 {VERDICT_TONE[worst]}" aria-busy={checking}>
    <p class="font-medium">{verdict}</p>
    <p class="mt-0.5 text-xs text-fg-muted">
      {#if checkedAt}Checked at {checkedAt}.{/if}
      {#if report.package_manager}
        Commands are for {report.package_manager}; OdHive never runs them.
      {:else}
        Your package manager is unknown, so no install commands are shown.
      {/if}
    </p>
  </div>

  <div class="mt-4 grid grid-cols-1 gap-4 xl:grid-cols-2" class:opacity-60={checking}>
    {#each report.checks as check (check.id)}
      <CheckCard {check} manager={report.package_manager ?? null} />
    {/each}
  </div>
{:else if checking}
  <p class="mt-6 text-sm text-fg-muted">Checking PostgreSQL, Python, wkhtmltopdf, git and uv…</p>
{/if}
