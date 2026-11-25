<script lang="ts">
  import { onDestroy, tick } from 'svelte'
  import type { ApiClient } from '../lib/api/client'
  import { ConfigForm } from '../lib/config/form.svelte'
  import OptionField from '../lib/config/OptionField.svelte'
  import { ConfPreview } from '../lib/config/preview.svelte'
  import { download } from '../lib/logs/export'
  import { routeHref, router } from '../lib/router.svelte'
  import { api as defaultApi } from '../lib/stores'

  let {
    project,
    api = defaultApi,
  }: {
    project: string
    api?: Pick<
      ApiClient,
      'projectSchema' | 'getProjectOptions' | 'putProjectOptions' | 'previewProjectOptions' | 'importProjectOptions' | 'revealProjectSecret'
    >
  } = $props()

  // The page is keyed by project (App.svelte), so one form per mount.
  // svelte-ignore state_referenced_locally
  const form = new ConfigForm(api, project)
  void form.load()
  // svelte-ignore state_referenced_locally
  const preview = new ConfPreview(api, project)
  onDestroy(() => preview.dispose())

  // A new preview whenever the options change (and once loaded, then after a save).
  $effect(() => {
    if (form.status !== 'ready') return
    const update = form.update
    if (form.problemCount > 0) preview.hold('The preview waits until the marked options are fixed.')
    else preview.request(update)
  })

  let previewTab: 'file' | 'changes' = $state('file')
  let fileInput: HTMLInputElement | undefined = $state()

  async function importChosen(event: Event & { currentTarget: HTMLInputElement }): Promise<void> {
    const input = event.currentTarget
    const file = input.files?.[0]
    input.value = ''
    if (!file) return
    if (await form.importFile(file)) previewTab = 'changes'
  }

  function downloadPreview(): void {
    download({ name: 'odoo.conf', type: 'text/plain;charset=utf-8', text: preview.text })
  }

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
  const tab = 'inline-flex min-h-8 items-center rounded-md px-2.5 text-sm hover:bg-surface-sunken'
  const tabOn = 'bg-accent-soft font-medium'
</script>

<svelte:window {onbeforeunload} />

<div class="flex flex-col gap-1">
  <div class="flex flex-wrap items-center justify-between gap-2">
    <h1 class="text-2xl font-semibold">Odoo options</h1>
    {#if form.status === 'ready'}
      <input
        bind:this={fileInput}
        type="file"
        accept=".conf,.cfg,.ini,text/plain"
        class="hidden"
        tabindex="-1"
        aria-hidden="true"
        onchange={importChosen}
        data-testid="import-file"
      />
      <button type="button" class={button} disabled={form.importing} onclick={() => fileInput?.click()}>
        {form.importing ? 'Importing…' : 'Import odoo.conf…'}
      </button>
    {/if}
  </div>
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

  {#if form.importError}
    <p class="mt-4 text-sm text-state-crashed" role="alert">Import failed: {form.importError}</p>
  {/if}
  {#if form.importNotes.length > 0}
    <div class="mt-4 rounded-md border border-line bg-surface-sunken p-3 text-sm" role="status" data-testid="import-notes">
      <div class="flex items-start gap-2">
        <p class="flex-1">Imported into the form; review the changes and save. Not taken over:</p>
        <button type="button" class="{button} min-h-8 px-2" onclick={() => (form.importNotes = [])}>Dismiss</button>
      </div>
      <ul class="mt-2 list-disc pl-5 text-fg-muted">
        {#each form.importNotes as note, index (index)}
          <li class="break-words">{note}</li>
        {/each}
      </ul>
    </div>
  {/if}

  <div class="mt-4 grid grid-cols-[minmax(0,1fr)] items-start gap-4 xl:grid-cols-[minmax(0,1fr)_minmax(0,32rem)]">
  <aside
    class="flex min-w-0 flex-col rounded-lg border border-line bg-surface-raised xl:sticky xl:top-18 xl:order-2 xl:max-h-[calc(100dvh-10rem)]"
    aria-labelledby="preview-title"
  >
    <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-2">
      <h2 id="preview-title" class="font-semibold">odoo.conf</h2>
      <div class="flex gap-1" role="group" aria-label="Show">
        <button type="button" class="{tab} {previewTab === 'file' ? tabOn : ''}" aria-pressed={previewTab === 'file'} onclick={() => (previewTab = 'file')}
          >Preview</button
        >
        <button
          type="button"
          class="{tab} {previewTab === 'changes' ? tabOn : ''}"
          aria-pressed={previewTab === 'changes'}
          onclick={() => (previewTab = 'changes')}>Changes ({preview.changes})</button
        >
      </div>
      <button type="button" class="{button} ml-auto min-h-8 px-2" disabled={preview.text === ''} onclick={downloadPreview}>Download</button>
    </div>
    <p class="px-4 pt-2 text-xs text-fg-muted">
      As the next start writes it to <span class="font-mono break-all">{form.confPath}</span>; secret values are masked.
    </p>
    <p class="px-4 pt-1 text-xs empty:hidden {preview.error ? 'text-state-crashed' : 'text-fg-muted'}" aria-live="polite">
      {#if preview.error}{preview.error}{:else if preview.pending}Updating…{/if}
    </p>
    <!-- Scrollable, so it takes focus for keyboard scrolling (axe scrollable-region-focusable). -->
    {#if previewTab === 'file'}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <pre
        class="max-h-72 min-h-24 overflow-auto px-4 py-3 font-mono text-xs leading-5 xl:max-h-none"
        tabindex="0"
        role="region"
        aria-label="odoo.conf preview"
        data-testid="conf-preview">{preview.text}</pre>
    {:else}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <div
        class="max-h-72 min-h-24 overflow-auto px-4 py-3 font-mono text-xs leading-5 xl:max-h-none"
        tabindex="0"
        role="region"
        aria-label="Changes against the saved odoo.conf"
        data-testid="conf-diff"
      >
        {#if preview.changes === 0}
          <p class="font-sans text-sm text-fg-muted">No changes against the saved options.</p>
        {:else}
          {#each preview.diff as line, index (index)}
            <div
              class="whitespace-pre {line.kind === 'add' ? 'text-state-running' : line.kind === 'del' ? 'text-state-crashed' : 'text-fg-muted'}"
              data-kind={line.kind}
            >
              <span aria-hidden="true">{line.kind === 'add' ? '+' : line.kind === 'del' ? '-' : ' '}</span><span class="sr-only"
                >{line.kind === 'add' ? 'added: ' : line.kind === 'del' ? 'removed: ' : ''}</span
              >{line.text}
            </div>
          {/each}
        {/if}
      </div>
    {/if}
  </aside>

  <form class="flex min-w-0 flex-col gap-4 xl:order-1" onsubmit={save} novalidate>
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
          Saved to odoo.conf. Running instances use the new options after a restart.
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
  </div>
{/if}
