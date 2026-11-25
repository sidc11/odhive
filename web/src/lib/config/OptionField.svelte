<script lang="ts">
  import { tick } from 'svelte'
  import type { OdooOption } from '../api'
  import { formatDefault, selectValues, widgetOf } from './fields'
  import type { ConfigForm } from './form.svelte'

  let { form, option }: { form: ConfigForm; option: OdooOption } = $props()

  const id = $derived(`opt-${option.key}`)
  const secret = $derived(form.isSecret(option))
  const widget = $derived(widgetOf(option, secret))
  const value = $derived(form.values[option.key] ?? '')
  const fallback = $derived(formatDefault(option.default))
  const problem = $derived(form.problems[option.key])
  const edit = $derived(form.secrets[option.key] ?? { action: 'keep' as const })
  const savedSecret = $derived(form.secretsSet.includes(option.key))
  /** The password input shows: no saved value, or replacing it. */
  const typingSecret = $derived(edit.action === 'set' || (edit.action === 'keep' && !savedSecret))
  /** The saved value, when the user revealed it. */
  const shown = $derived(form.revealed[option.key])
  const revealError = $derived(form.revealErrors[option.key])
  /** Show the typed new value as plain text. */
  let showTyped = $state(false)

  const describedBy = $derived(
    [problem ? `${id}-problem` : null, revealError ? `${id}-reveal` : null, option.help ? `${id}-help` : null, `${id}-default`].filter(Boolean).join(' '),
  )

  function defaultText(): string {
    if (secret) return savedSecret ? 'A value is saved; it is shown only when you reveal it' : 'Not set'
    const text = fallback === null ? 'No default' : fallback === '' ? 'Default: empty' : `Default: ${fallback}`
    return option.type === 'list' ? `${text} · comma-separated` : text
  }

  let field: HTMLInputElement | undefined = $state()

  async function replace(): Promise<void> {
    form.setSecret(option.key, { action: 'set', value: '' })
    await tick()
    field?.focus()
  }

  const input =
    'min-h-9 w-full rounded-md border border-line bg-surface px-3 font-mono text-sm placeholder:text-fg-muted aria-invalid:border-state-crashed'
  const small =
    'inline-flex min-h-8 items-center rounded-md border border-line px-2.5 text-sm hover:bg-surface-sunken disabled:opacity-50'
</script>

<div class="flex flex-col gap-1.5 py-3" data-option={option.key}>
  <div class="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
    {#if secret && !typingSecret}
      <span id="{id}-label" class="font-mono text-sm font-medium break-all">{option.key}</span>
    {:else}
      <label for={id} class="font-mono text-sm font-medium break-all">{option.key}</label>
    {/if}
    {#if option.flags.length > 0}
      <span class="font-mono text-xs text-fg-muted">{option.flags.join(', ')}</span>
    {/if}
    {#if form.isSet(option) && !secret}
      <span class="rounded bg-accent-soft px-1.5 text-xs">set</span>
    {/if}
  </div>

  <div class="flex items-center gap-2">
    {#if widget === 'secret'}
      {#if typingSecret}
        <input
          bind:this={field}
          {id}
          type={showTyped ? 'text' : 'password'}
          class={input}
          value={edit.action === 'set' ? edit.value : ''}
          oninput={(e) => form.setSecret(option.key, { action: 'set', value: e.currentTarget.value })}
          autocomplete="new-password"
          spellcheck="false"
          placeholder={savedSecret ? 'New value' : 'Not set'}
          aria-invalid={Boolean(problem)}
          aria-describedby={describedBy}
        />
        <button
          type="button"
          class={small}
          aria-label="{showTyped ? 'Hide' : 'Show'} typed {option.key}"
          aria-pressed={showTyped}
          onclick={() => (showTyped = !showTyped)}>{showTyped ? 'Hide' : 'Show'}</button
        >
        {#if savedSecret}
          <button type="button" class={small} onclick={() => form.setSecret(option.key, { action: 'keep' })}>Keep saved</button>
        {/if}
      {:else}
        {#if shown !== undefined}
          <span {id} class="min-w-0 flex-1 font-mono text-sm break-all" data-revealed>{shown}</span>
        {:else}
          <span {id} class="flex-1 font-mono text-sm text-fg-muted">
            {edit.action === 'remove' ? 'Removed when you save' : '••••••••'}
          </span>
        {/if}
        {#if edit.action === 'remove'}
          <button type="button" class={small} onclick={() => form.setSecret(option.key, { action: 'keep' })}>Undo</button>
        {:else}
          <button
            type="button"
            class={small}
            aria-label="{shown !== undefined ? 'Hide' : 'Reveal'} {option.key}"
            aria-pressed={shown !== undefined}
            disabled={form.revealing === option.key}
            onclick={() => (shown !== undefined ? form.hide(option.key) : form.reveal(option.key))}
            >{shown !== undefined ? 'Hide' : 'Reveal'}</button
          >
          <button type="button" class={small} aria-label="Replace {option.key}" onclick={replace}>Replace</button>
          <button
            type="button"
            class={small}
            aria-label="Remove {option.key}"
            onclick={() => form.setSecret(option.key, { action: 'remove' })}>Remove</button
          >
        {/if}
      {/if}
    {:else if widget === 'bool' || widget === 'choice'}
      <select
        {id}
        class="{input} font-sans"
        {value}
        onchange={(e) => form.setValue(option.key, e.currentTarget.value)}
        aria-invalid={Boolean(problem)}
        aria-describedby={describedBy}
      >
        <option value="">Default{fallback ? ` (${fallback})` : ''}</option>
        {#each selectValues(option, value) as choice (choice)}
          <option value={choice}>{choice}</option>
        {/each}
      </select>
    {:else}
      <input
        {id}
        type="text"
        class={input}
        {value}
        oninput={(e) => form.setValue(option.key, e.currentTarget.value)}
        inputmode={widget === 'number' ? (option.type === 'int' ? 'numeric' : 'decimal') : undefined}
        autocomplete="off"
        spellcheck="false"
        placeholder={fallback || option.metavar || ''}
        aria-invalid={Boolean(problem)}
        aria-describedby={describedBy}
      />
    {/if}
    {#if !secret && value !== ''}
      <button type="button" class={small} aria-label="Reset {option.key} to default" onclick={() => form.setValue(option.key, '')}
        >Reset</button
      >
    {/if}
  </div>

  {#if problem}
    <p id="{id}-problem" class="text-sm text-state-crashed">{problem}</p>
  {/if}
  {#if revealError}
    <p id="{id}-reveal" class="text-sm text-state-crashed">Could not reveal: {revealError}</p>
  {/if}
  {#if option.help}
    <p id="{id}-help" class="text-sm text-fg-muted">{option.help}</p>
  {/if}
  <p id="{id}-default" class="text-xs text-fg-muted">
    {defaultText()}
  </p>
</div>
