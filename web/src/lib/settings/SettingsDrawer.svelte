<script lang="ts">
  import { tick } from 'svelte'
  import type { ApiClient } from '../api/client'
  import type { InstanceActions } from '../actions.svelte'
  import { trapTab } from '../focus'
  import type { ProjectsStore } from '../projects.svelte'
  import { router } from '../router.svelte'
  import { allowedActions, instanceState } from '../status'
  import {
    actions as defaultActions,
    api as defaultApi,
    projects as defaultProjects,
    settingsDrawer as defaultDrawer,
  } from '../stores'
  import { AUTOFIX_MODES, RELOAD_MODES } from './rules'
  import { SettingsForm, type SettingsDrawerStore } from './settings.svelte'

  let {
    drawer = defaultDrawer,
    api = defaultApi,
    projects = defaultProjects,
    actions = defaultActions,
  }: {
    drawer?: SettingsDrawerStore
    api?: Pick<ApiClient, 'getInstanceSettings' | 'putInstanceSettings'>
    projects?: Pick<ProjectsStore, 'find' | 'load'>
    actions?: Pick<InstanceActions, 'run' | 'busy'>
  } = $props()

  let form: SettingsForm | null = $state(null)
  let confirmingClose = $state(false)
  let panel: HTMLElement | undefined = $state()

  const target = $derived(drawer.target)
  const project = $derived(target ? projects.find(target.project) : undefined)
  const view = $derived(target ? project?.instances.find((i) => i.id === target.instance) : undefined)
  const status = $derived(view ? instanceState(view) : 'stopped')
  const canRestart = $derived(project && view ? allowedActions(project, view).restart : false)
  const busy = $derived(target ? actions.busy(target.project, target.instance) : undefined)

  // A fresh form for each instance the drawer opens on.
  $effect(() => {
    const t = target
    confirmingClose = false
    if (!t) {
      form = null
      return
    }
    const next = new SettingsForm(api, t.project, t.instance)
    form = next
    void next.load()
    void tick().then(() => panel?.focus())
  })

  // It belongs to the page it was opened on (e.g. browser back closes it).
  let openedAt = ''
  $effect(() => {
    const path = router.path
    if (!target) {
      openedAt = path
    } else if (path !== openedAt) {
      drawer.hide()
    }
  })

  function requestClose(): void {
    if (form?.dirty && !confirmingClose) {
      confirmingClose = true
      void tick().then(() => panel?.querySelector<HTMLElement>('[data-keep-editing]')?.focus())
      return
    }
    drawer.hide()
  }

  function keepEditing(): void {
    confirmingClose = false
    void tick().then(() => panel?.focus())
  }

  async function save(event: SubmitEvent): Promise<void> {
    event.preventDefault()
    if (!form) return
    confirmingClose = false
    if (await form.save()) void projects.load()
  }

  function onkeydown(event: KeyboardEvent): void {
    if (!target || !panel) return
    if (event.key === 'Escape') {
      event.preventDefault()
      if (confirmingClose) keepEditing()
      else requestClose()
    } else {
      trapTab(event, panel)
    }
  }

  const ARGS_PLACEHOLDER = ['--dev=xml', '--log-level=debug'].join('\n')

  const input =
    'min-h-9 w-full rounded-md border border-line bg-surface px-3 text-sm placeholder:text-fg-muted aria-invalid:border-state-crashed disabled:opacity-50'
  const button =
    'inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const primary = `${button} border-transparent bg-accent text-accent-fg hover:bg-accent hover:opacity-90`
  const option =
    'flex cursor-pointer items-start gap-3 rounded-md border border-line p-3 has-checked:border-accent has-checked:bg-accent-soft'
</script>

<svelte:window {onkeydown} />

{#if target}
  <div class="fixed inset-0 z-40">
    <button
      type="button"
      class="absolute inset-0 size-full cursor-default bg-backdrop"
      aria-label="Close settings"
      tabindex="-1"
      onclick={requestClose}
    ></button>
    <div
      bind:this={panel}
      role="dialog"
      aria-modal="true"
      aria-labelledby="settings-title"
      aria-describedby="settings-subtitle"
      tabindex="-1"
      class="absolute inset-y-0 right-0 flex w-full max-w-md flex-col border-l border-line bg-surface-raised shadow-xl focus:outline-none"
      data-testid="settings-drawer"
    >
      <header class="flex items-start gap-2 border-b border-line p-4">
        <div class="min-w-0 flex-1">
          <h2 id="settings-title" class="text-lg font-semibold">Instance settings</h2>
          <p id="settings-subtitle" class="truncate text-sm text-fg-muted">{target.project} / {target.instance}</p>
        </div>
        <button type="button" class="{button} px-2" aria-label="Close" onclick={requestClose}>
          <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
            ><path d="M6 6l12 12M18 6L6 18" /></svg
          >
        </button>
      </header>

      {#if !form || form.status === 'loading'}
        <p class="p-4 text-sm text-fg-muted">Loading settings…</p>
      {:else if form.status === 'error'}
        <div class="flex flex-col items-start gap-3 p-4">
          <p class="text-sm text-state-crashed" role="alert">{form.loadError}</p>
          <button type="button" class={button} onclick={() => form?.load()}>Try again</button>
        </div>
      {:else}
        {@const f = form}
        <form class="flex min-h-0 flex-1 flex-col" onsubmit={save} novalidate>
          <div class="flex flex-1 flex-col gap-6 overflow-y-auto p-4">
            <fieldset class="flex flex-col gap-2">
              <legend class="mb-2 text-sm font-medium">Smart Reload</legend>
              {#each RELOAD_MODES as mode (mode.value)}
                <label class={option}>
                  <input type="radio" name="reload_mode" class="mt-1 accent-accent" value={mode.value} bind:group={f.reloadMode} />
                  <span class="min-w-0">
                    <span class="block text-sm font-medium">{mode.label}</span>
                    <span class="block text-sm text-fg-muted">{mode.description}</span>
                  </span>
                </label>
              {/each}
            </fieldset>

            <fieldset class="flex flex-col gap-2">
              <legend class="mb-2 text-sm font-medium">Missing Python packages</legend>
              {#each AUTOFIX_MODES as mode (mode.value)}
                <label class={option}>
                  <input type="radio" name="autofix_mode" class="mt-1 accent-accent" value={mode.value} bind:group={f.autofixMode} />
                  <span class="min-w-0">
                    <span class="block text-sm font-medium">{mode.label}</span>
                    <span class="block text-sm text-fg-muted">{mode.description}</span>
                  </span>
                </label>
              {/each}
            </fieldset>

            <fieldset class="flex flex-col gap-2">
              <legend class="mb-2 text-sm font-medium">SQL log</legend>
              <label class={option}>
                <input type="checkbox" name="sql_log" class="mt-1 accent-accent" bind:checked={f.sqlLog} />
                <span class="min-w-0">
                  <span class="block text-sm font-medium">Log SQL queries</span>
                  <span class="block text-sm text-fg-muted">
                    Odoo logs every query it runs (<code>odoo.sql_db:DEBUG</code>), shown as the SQL source in the log viewer.
                    Busy servers log a lot.
                  </span>
                </span>
              </label>
            </fieldset>

            <div class="flex flex-col gap-2">
              <label for="settings-args" class="text-sm font-medium">Extra odoo-bin arguments</label>
              <textarea
                id="settings-args"
                class="{input} min-h-24 py-2 font-mono"
                bind:value={f.argsText}
                rows="4"
                autocomplete="off"
                spellcheck="false"
                placeholder={ARGS_PLACEHOLDER}
                aria-describedby="settings-args-help{f.argNotes.length > 0 ? ' settings-args-notes' : ''}"
                aria-invalid={f.argNotes.some((n) => n.blocking)}
              ></textarea>
              <p id="settings-args-help" class="text-sm text-fg-muted">
                One argument per line, passed as is (no shell). Ports, database, addons path and workers are set by OdHive.
              </p>
              {#if f.argNotes.length > 0}
                <ul id="settings-args-notes" class="flex flex-col gap-1 text-sm">
                  {#each f.argNotes as note, index (index)}
                    <li class={note.blocking ? 'text-state-crashed' : 'text-fg-muted'}>
                      <code class="break-all">{note.arg}</code>
                      {note.message}
                    </li>
                  {/each}
                </ul>
              {/if}
            </div>

            <fieldset class="flex flex-col gap-2">
              <legend class="mb-2 text-sm font-medium">Environment variables</legend>
              <p class="text-sm text-fg-muted">
                For this instance only.
                {#if f.projectEnv.length > 0}
                  It also gets the project's {f.projectEnv.join(', ')}; a variable here with the same name replaces it.
                {/if}
              </p>
              {#if f.envRows.length > 0}
                <ul class="flex flex-col gap-3">
                  {#each f.envRows as row, index (row.id)}
                    {@const problem = f.envProblems[row.id]}
                    {@const shown = problem && (f.tried || row.key.trim() !== '')}
                    <li class="flex flex-col gap-1">
                      <div class="grid grid-cols-[minmax(0,2fr)_minmax(0,3fr)_auto] gap-2">
                        <input
                          class="{input} font-mono"
                          bind:value={row.key}
                          aria-label="Name of variable {index + 1}"
                          placeholder="NAME"
                          autocomplete="off"
                          spellcheck="false"
                          autocapitalize="characters"
                          aria-invalid={Boolean(shown)}
                          aria-describedby={shown ? `settings-env-${row.id}-problem` : undefined}
                          data-env-key
                        />
                        <input
                          class="{input} font-mono"
                          bind:value={row.value}
                          aria-label="Value of {row.key.trim() || `variable ${index + 1}`}"
                          placeholder="value"
                          autocomplete="off"
                          spellcheck="false"
                        />
                        <button
                          type="button"
                          class="{button} px-2"
                          aria-label="Remove {row.key.trim() || `variable ${index + 1}`}"
                          onclick={() => f.removeEnvRow(row.id)}
                        >
                          <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
                            ><path d="M6 6l12 12M18 6L6 18" /></svg
                          >
                        </button>
                      </div>
                      {#if shown}
                        <p id="settings-env-{row.id}-problem" class="text-sm text-state-crashed">{problem}</p>
                      {:else if f.overridden.includes(row.key.trim())}
                        <p class="text-sm text-fg-muted">Replaces the project's {row.key.trim()}.</p>
                      {/if}
                    </li>
                  {/each}
                </ul>
              {/if}
              <button
                type="button"
                class="{button} self-start"
                onclick={() => {
                  f.addEnvRow()
                  void tick().then(() => {
                    const keys = panel?.querySelectorAll<HTMLElement>('[data-env-key]')
                    keys?.[keys.length - 1]?.focus()
                  })
                }}>Add variable</button
              >
            </fieldset>
          </div>

          <footer class="flex flex-col gap-3 border-t border-line p-4">
            <div role="status" class="text-sm empty:hidden">
              {#if f.savedNow}
                {#if status === 'external'}
                  Saved. This instance runs outside OdHive's service; stop it and start it again to use the new settings.
                {:else if canRestart}
                  <span class="flex flex-wrap items-center gap-2">
                    <span class="flex-1">Saved. Restart {target.instance} to use the new settings.</span>
                    <button
                      type="button"
                      class={button}
                      disabled={busy !== undefined}
                      onclick={() => void actions.run('restart', target.project, target.instance)}
                    >
                      {busy === 'restart' ? 'Restarting…' : 'Restart now'}
                    </button>
                  </span>
                {:else}
                  Saved. The new settings are used at the next start.
                {/if}
              {/if}
            </div>
            {#if f.saveError}
              <p class="text-sm text-state-crashed" role="alert">{f.saveError}</p>
            {/if}
            {#if confirmingClose}
              <div class="flex flex-wrap items-center gap-2 rounded-md bg-surface-sunken p-3" role="alert">
                <p class="flex-1 text-sm">Discard unsaved changes?</p>
                <button type="button" class={button} data-keep-editing onclick={keepEditing}>Keep editing</button>
                <button type="button" class={button} onclick={() => drawer.hide()}>Discard</button>
              </div>
            {:else}
              <div class="flex justify-end gap-2">
                <button type="button" class={button} onclick={requestClose}>Cancel</button>
                <button type="submit" class={primary} disabled={f.saving || !f.dirty}>
                  {f.saving ? 'Saving…' : 'Save'}
                </button>
              </div>
            {/if}
          </footer>
        </form>
      {/if}
    </div>
  </div>
{/if}
