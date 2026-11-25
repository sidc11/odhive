<script lang="ts">
  import { tick } from 'svelte'
  import type { ApiClient } from '../lib/api/client'
  import { ConfigForm } from '../lib/config/form.svelte'
  import OptionField from '../lib/config/OptionField.svelte'
  import { routeHref, router } from '../lib/router.svelte'
  import { api as defaultApi } from '../lib/stores'

  let {
    project,
    api = defaultApi,
  }: {
    project: string
    api?: Pick<ApiClient, 'projectSchema' | 'getProjectOptions' | 'putProjectOptions'>
  } = $props()

  // The page is keyed by project (App.svelte), so one form per mount.
  // svelte-ignore state_referenced_locally
  const form = new ConfigForm(api, project)
  void form.load()

  /** Sections the user folded; a search shows every section. */
  let folded: string[] = $state([])
  let customList: HTMLElement | undefined = $state()

  const searching = $derived(form.query.trim() !== '' || form.onlySet)

  function toggle(id: string): void {
    folded = folded.includes(id) ? folded.filter((f) => f !== id) : [...folded, id]
  }

  async function save(event: SubmitEvent): Promise<void> {
    event.preventDefault()
    if (await form.save()) return
    if (form.problemCount > 0) {
      await tick()
      document.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus()
    }
  }

  async function addRow(): Promise<void> {
    form.addRow()
    await tick()
    const keys = customList?.querySelectorAll<HTMLElement>('[data-custom-key]')
    keys?.[keys.length - 1]?.focus()
  }

  function onbeforeunload(event: BeforeUnloadEvent): void {
    if (form.dirty) event.preventDefault()
  }

  const input =
    'min-h-9 w-full rounded-md border border-line bg-surface px-3 text-sm placeholder:text-fg-muted aria-invalid:border-state-crashed'
  const button =
    'inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const primary = `${button} border-transparent bg-accent text-accent-fg hover:bg-accent hover:opacity-90`
</script>

<svelte:window {onbeforeunload} />

<div class="flex flex-col gap-1">
  <h1 class="text-2xl font-semibold">Odoo options</h1>
  <p class="text-sm text-fg-muted">
    <a href={routeHref({ name: 'project', project })} class="underline" onclick={router.link}>{project}</a>
    {#if form.series}· Odoo {form.series}{/if} · written to odoo.conf at each start
  </p>
</div>

{#if form.status === 'loading'}
  <p class="mt-6 text-sm text-fg-muted">Loading options…</p>
{:else if form.status === 'error'}
  <div class="mt-6 flex flex-col items-start gap-3">
    <p class="text-sm text-state-crashed" role="alert">{form.loadError}</p>
    <button type="button" class={button} onclick={() => form.load()}>Try again</button>
  </div>
{:else}
  {#if form.schemaSource === 'bundled'}
    <div class="mt-4 rounded-md border border-line bg-surface-sunken p-3 text-sm" data-testid="bundled-schema">
      <p>
        Showing the option list bundled with OdHive for Odoo {form.series}: the options of this project's own Odoo could
        not be read.
      </p>
      {#if form.schemaReason}
        <details class="mt-1">
          <summary class="cursor-pointer text-fg-muted">Why</summary>
          <pre class="mt-2 max-h-48 overflow-auto font-mono text-xs whitespace-pre-wrap text-fg-muted">{form.schemaReason}</pre>
        </details>
      {/if}
    </div>
  {/if}

  <form class="mt-4 flex flex-col gap-4" onsubmit={save} novalidate>
    <div class="flex flex-col gap-2 sm:flex-row sm:items-center">
      <label for="options-search" class="sr-only">Search options</label>
      <input
        id="options-search"
        type="search"
        class="{input} sm:max-w-sm"
        bind:value={form.query}
        placeholder="Search options, flags or help"
        autocomplete="off"
        spellcheck="false"
        aria-describedby="options-count"
      />
      <label class="flex min-h-9 items-center gap-2 text-sm">
        <input type="checkbox" class="accent-accent" bind:checked={form.onlySet} />
        Only options with a value
      </label>
      <p id="options-count" class="text-sm text-fg-muted sm:ml-auto" aria-live="polite">
        {form.shownCount} of {form.options.length} options
      </p>
    </div>

    <p class="text-sm text-fg-muted">
      Set by OdHive and not editable here: <span class="font-mono">{form.managedKeys.join(', ')}</span>. Command-line
      only options go in an instance's extra arguments.
    </p>

    {#if form.shownCount === 0}
      <p class="text-sm text-fg-muted">No option matches.</p>
    {/if}

    {#each form.shownSections as section (section.id)}
      {@const open = searching || !folded.includes(section.id)}
      <section class="rounded-lg border border-line bg-surface-raised" aria-labelledby="{section.id}-title">
        <h2 id="{section.id}-title">
          <button
            type="button"
            class="flex w-full items-center gap-2 px-4 py-3 text-left font-semibold hover:bg-surface-sunken"
            aria-expanded={open}
            aria-controls={section.id}
            disabled={searching}
            onclick={() => toggle(section.id)}
          >
            <svg
              viewBox="0 0 24 24"
              class="size-4 shrink-0 {open ? 'rotate-90' : ''}"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
              aria-hidden="true"><path d="M9 6l6 6-6 6" /></svg
            >
            <span class="flex-1">{section.title}</span>
            <span class="text-sm font-normal text-fg-muted">{section.options.length}</span>
          </button>
        </h2>
        <div id={section.id} class="divide-y divide-line border-t border-line px-4" hidden={!open}>
          {#each section.options as option (option.key)}
            <OptionField {form} {option} />
          {/each}
        </div>
      </section>
    {/each}

    {#if !searching}
      <section class="rounded-lg border border-line bg-surface-raised p-4" aria-labelledby="custom-options-title">
        <h2 id="custom-options-title" class="font-semibold">Other options</h2>
        <p class="mt-1 text-sm text-fg-muted">Keys this Odoo does not know, e.g. read by third-party modules.</p>
        {#if form.customRows.length > 0}
          <ul class="mt-3 flex flex-col gap-3" bind:this={customList}>
            {#each form.customRows as row, index (row.id)}
              {@const problem = form.rowProblems[row.id]}
              {@const shown = problem && (form.tried || row.key.trim() !== '')}
              <li class="flex flex-col gap-1">
                <div class="grid grid-cols-[minmax(0,2fr)_minmax(0,3fr)_auto] gap-2">
                  <input
                    class="{input} font-mono"
                    bind:value={row.key}
                    aria-label="Key of option {index + 1}"
                    placeholder="key"
                    autocomplete="off"
                    spellcheck="false"
                    aria-invalid={Boolean(shown)}
                    aria-describedby={shown ? `custom-${row.id}-problem` : undefined}
                    data-custom-key
                  />
                  <input
                    class="{input} font-mono"
                    bind:value={row.value}
                    aria-label="Value of {row.key.trim() || `option ${index + 1}`}"
                    placeholder="value"
                    autocomplete="off"
                    spellcheck="false"
                  />
                  <button
                    type="button"
                    class="{button} px-2"
                    aria-label="Remove {row.key.trim() || `option ${index + 1}`}"
                    onclick={() => form.removeRow(row.id)}
                  >
                    <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
                      ><path d="M6 6l12 12M18 6L6 18" /></svg
                    >
                  </button>
                </div>
                {#if shown}
                  <p id="custom-{row.id}-problem" class="text-sm text-state-crashed">{problem}</p>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
        <button type="button" class="{button} mt-3" onclick={addRow}>Add option</button>
      </section>
    {/if}

    <footer
      class="sticky bottom-0 -mx-4 flex flex-col gap-2 border-t border-line bg-surface-raised px-4 py-3 sm:flex-row sm:items-center md:-mx-6 md:px-6"
    >
      <div class="flex-1 text-sm" role="status">
        {#if form.savedNow}
          Saved. Running instances use the new options after a restart.
        {:else if form.tried && form.problemCount > 0}
          <span class="text-state-crashed">Fix {form.problemCount === 1 ? 'the marked option' : `the ${form.problemCount} marked options`} first.</span>
        {:else if form.dirty}
          Unsaved changes.
        {/if}
      </div>
      {#if form.saveError}
        <p class="text-sm text-state-crashed" role="alert">{form.saveError}</p>
      {/if}
      <div class="flex justify-end gap-2">
        <button type="button" class={button} disabled={!form.dirty || form.saving} onclick={() => form.discard()}>Discard</button>
        <button type="submit" class={primary} disabled={form.saving || !form.dirty}>
          {form.saving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </footer>
  </form>
{/if}
