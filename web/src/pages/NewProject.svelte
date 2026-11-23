<script lang="ts">
  import { tick } from 'svelte'
  import { routeHref, router } from '../lib/router.svelte'
  import { api, jobs, projects } from '../lib/stores'
  import SetupProgress from '../lib/wizard/SetupProgress.svelte'
  import { cloneSummary } from '../lib/wizard/progress'
  import { CLONE_VERSIONS, ProjectWizard, STEP_LABELS, STEPS } from '../lib/wizard/wizard.svelte'

  let { wizard = new ProjectWizard(api, jobs, projects) }: { wizard?: ProjectWizard } = $props()

  let heading = $state<HTMLHeadingElement>()
  let communityPath = $state('')
  let enterprisePath = $state('')
  let cloneVersion = $state<string>(CLONE_VERSIONS[CLONE_VERSIONS.length - 1] ?? '19.0')
  let customPython = $state('')
  /** A step's problem is shown once Next was tried on it. */
  let tried = $state(false)

  const phase = $derived(wizard.phase)
  const problem = $derived(wizard.problem())
  const cloneJob = $derived(wizard.job(wizard.cloneJob))
  const setupJob = $derived('job' in phase ? wizard.job(phase.job) : undefined)
  const otherPython = $derived(
    wizard.python !== null &&
      !(wizard.pythons.status === 'ready' && wizard.pythons.value.interpreters.some((i) => i.path === wizard.python)),
  )

  // Move focus to the new step's heading so keyboard and screen-reader
  // users follow along.
  let shown = ''
  $effect(() => {
    const key = `${wizard.step}:${phase.name}`
    if (key === shown) return
    const first = shown === ''
    shown = key
    tried = false
    if (!first) void tick().then(() => heading?.focus())
  })

  function submit(event: SubmitEvent): void {
    event.preventDefault()
    tried = true
    if (wizard.isLast) void wizard.create()
    else wizard.next()
  }

  /** Enter in a side input acts on that input, not on the step. */
  function onEnter(action: () => unknown) {
    return (event: KeyboardEvent): void => {
      if (event.key !== 'Enter') return
      event.preventDefault()
      void action()
    }
  }

  async function registerCommunity(): Promise<void> {
    if (await wizard.register(communityPath, 'community')) communityPath = ''
  }

  async function registerEnterprise(): Promise<void> {
    if (await wizard.register(enterprisePath, 'enterprise')) enterprisePath = ''
  }

  function chooseOtherPython(): void {
    wizard.python = customPython
  }

  function openProject(name: string): void {
    router.navigate(routeHref({ name: 'project', project: name }))
  }

  const input =
    'min-h-9 w-full rounded-md border border-line bg-surface px-3 text-sm placeholder:text-fg-muted disabled:opacity-50'
  const button =
    'inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md border border-line px-3 text-sm font-medium hover:bg-surface-sunken disabled:cursor-not-allowed disabled:opacity-50'
  const primary = `${button} border-transparent bg-accent text-accent-fg hover:bg-accent hover:opacity-90`
  const option =
    'flex cursor-pointer items-start gap-3 rounded-md border border-line p-3 has-checked:border-accent has-checked:bg-accent-soft has-disabled:cursor-not-allowed has-disabled:opacity-60'
</script>

<h1 class="text-2xl font-semibold">New project</h1>

<div class="mt-4 flex flex-col gap-6 lg:flex-row lg:items-start">
  <nav aria-label="Wizard steps" class="lg:w-52 lg:shrink-0">
    <ol class="flex gap-1 overflow-x-auto pb-1 lg:flex-col lg:overflow-visible">
      {#each STEPS as id, index (id)}
        {@const current = phase.name === 'editing' && index === wizard.step}
        <li class="shrink-0">
          <button
            type="button"
            class="flex min-h-9 w-full items-center gap-2 rounded-md px-2 text-left text-sm whitespace-nowrap hover:bg-surface-sunken disabled:cursor-default disabled:hover:bg-transparent aria-[current=step]:bg-accent-soft aria-[current=step]:font-medium"
            aria-current={current ? 'step' : undefined}
            disabled={phase.name !== 'editing' || index > wizard.reached}
            onclick={() => wizard.goto(index)}
          >
            <span
              class="inline-flex size-6 shrink-0 items-center justify-center rounded-full border border-line text-xs tabular-nums"
              aria-hidden="true">{index + 1}</span
            >
            <span class={index > wizard.reached ? 'text-fg-muted' : ''}>{STEP_LABELS[id]}</span>
          </button>
        </li>
      {/each}
    </ol>
  </nav>

  <section class="min-w-0 flex-1 rounded-lg border border-line bg-surface-raised p-4 md:p-6" aria-labelledby="wizard-heading">
    {#if phase.name === 'editing' || phase.name === 'creating'}
      <form class="flex flex-col gap-4" onsubmit={submit} novalidate>
        <h2 id="wizard-heading" bind:this={heading} tabindex="-1" class="text-lg font-semibold">
          <span class="text-fg-muted">Step {wizard.step + 1} of {STEPS.length} ·</span>
          {STEP_LABELS[wizard.stepId]}
        </h2>

        {#if wizard.stepId === 'name'}
          <div class="flex flex-col gap-1.5">
            <label for="wiz-name" class="text-sm font-medium">Project name</label>
            <input
              id="wiz-name"
              class="{input} max-w-sm"
              bind:value={wizard.name}
              autocomplete="off"
              spellcheck="false"
              placeholder="shop"
              aria-describedby="wiz-name-help"
              aria-invalid={tried && problem !== null}
            />
            <p id="wiz-name-help" class="text-sm text-fg-muted">
              Lowercase letters, digits, - and _. The project folder and database names start with it.
            </p>
          </div>
        {:else if wizard.stepId === 'source'}
          <fieldset class="flex flex-col gap-2">
            <legend class="mb-2 text-sm font-medium">Odoo Community source</legend>
            {#if wizard.sources.status === 'loading' || wizard.sources.status === 'idle'}
              <p class="text-sm text-fg-muted">Loading sources…</p>
            {:else if wizard.sources.status === 'error'}
              <p class="text-sm text-state-crashed" role="alert">{wizard.sources.message}</p>
              <button type="button" class="{button} self-start" onclick={() => wizard.loadSources()}>Try again</button>
            {:else if wizard.communitySources.length === 0}
              <p class="text-sm text-fg-muted">No Odoo source registered yet. Register a folder or clone Community below.</p>
            {:else}
              {#each wizard.communitySources as source (source.id)}
                <label class={option}>
                  <input
                    type="radio"
                    name="community"
                    class="mt-1 accent-accent"
                    checked={wizard.community === source.id}
                    onchange={() => wizard.chooseCommunity(source.id)}
                  />
                  <span class="min-w-0">
                    <span class="block text-sm font-medium">Odoo {source.odoo_version} <span class="text-fg-muted">· {source.id}</span></span>
                    <span class="block text-sm break-all text-fg-muted">{source.path}</span>
                  </span>
                </label>
              {/each}
            {/if}
          </fieldset>

          {#if wizard.sources.status === 'ready'}
            <div class="grid gap-4 md:grid-cols-2">
              <div class="flex flex-col gap-1.5 rounded-md border border-dashed border-line p-3">
                <label for="wiz-community-path" class="text-sm font-medium">Use an existing folder</label>
                <input
                  id="wiz-community-path"
                  class={input}
                  bind:value={communityPath}
                  placeholder="/home/you/src/odoo"
                  spellcheck="false"
                  onkeydown={onEnter(registerCommunity)}
                />
                <button type="button" class="{button} self-start" disabled={wizard.registering} onclick={registerCommunity}>
                  {wizard.registering ? 'Checking…' : 'Register folder'}
                </button>
              </div>
              <div class="flex flex-col gap-1.5 rounded-md border border-dashed border-line p-3">
                <label for="wiz-clone" class="text-sm font-medium">Clone Odoo Community</label>
                <select id="wiz-clone" class={input} bind:value={cloneVersion} disabled={wizard.cloneJob !== null}>
                  {#each CLONE_VERSIONS as version (version)}
                    <option value={version}>Odoo {version}</option>
                  {/each}
                </select>
                {#if wizard.cloneJob !== null}
                  <p class="text-sm text-fg-muted" role="status">{cloneJob ? cloneSummary(cloneJob) : 'Starting…'}</p>
                  <button type="button" class="{button} self-start" onclick={() => wizard.cancelClone()}>Cancel clone</button>
                {:else}
                  <p class="text-xs text-fg-muted">A shallow clone from github.com/odoo/odoo into the workspace (about 1 GB).</p>
                  <button type="button" class="{button} self-start" onclick={() => wizard.clone(cloneVersion)}>Clone</button>
                {/if}
              </div>
            </div>
          {/if}
          {#if wizard.sourceError}
            <p class="text-sm text-state-crashed" role="alert">{wizard.sourceError}</p>
          {/if}
        {:else if wizard.stepId === 'enterprise'}
          <fieldset class="flex flex-col gap-2">
            <legend class="mb-2 text-sm font-medium">
              Enterprise addons for Odoo {wizard.communitySource?.odoo_version} <span class="font-normal text-fg-muted">(optional)</span>
            </legend>
            <label class={option}>
              <input
                type="radio"
                name="enterprise"
                class="mt-1 accent-accent"
                checked={wizard.enterprise === null}
                onchange={() => (wizard.enterprise = null)}
              />
              <span class="text-sm font-medium">None (Community only)</span>
            </label>
            {#each wizard.enterpriseSources as source (source.id)}
              <label class={option}>
                <input
                  type="radio"
                  name="enterprise"
                  class="mt-1 accent-accent"
                  checked={wizard.enterprise === source.id}
                  onchange={() => (wizard.enterprise = source.id)}
                />
                <span class="min-w-0">
                  <span class="block text-sm font-medium">{source.id}</span>
                  <span class="block text-sm break-all text-fg-muted">{source.path}</span>
                </span>
              </label>
            {/each}
          </fieldset>
          <div class="flex max-w-xl flex-col gap-1.5 rounded-md border border-dashed border-line p-3">
            <label for="wiz-enterprise-path" class="text-sm font-medium">Use an Enterprise folder</label>
            <input
              id="wiz-enterprise-path"
              class={input}
              bind:value={enterprisePath}
              placeholder="/home/you/src/enterprise"
              spellcheck="false"
              onkeydown={onEnter(registerEnterprise)}
            />
            <button type="button" class="{button} self-start" disabled={wizard.registering} onclick={registerEnterprise}>
              {wizard.registering ? 'Checking…' : 'Register folder'}
            </button>
          </div>
          {#if wizard.sourceError}
            <p class="text-sm text-state-crashed" role="alert">{wizard.sourceError}</p>
          {/if}
        {:else if wizard.stepId === 'addons'}
          <p class="text-sm text-fg-muted">
            The project's own <code class="rounded bg-surface-sunken px-1">addons/</code> folder always comes first. Add folders
            of modules you already have (absolute paths); they are used in this order.
          </p>
          {#if wizard.addons.length > 0}
            <ul class="flex flex-col gap-2">
              {#each { length: wizard.addons.length }, index (index)}
                <li class="flex gap-2">
                  <label for="wiz-addons-{index}" class="sr-only">Addons folder {index + 1}</label>
                  <input
                    id="wiz-addons-{index}"
                    class={input}
                    bind:value={wizard.addons[index]}
                    placeholder="/home/you/src/my-addons"
                    spellcheck="false"
                  />
                  <button type="button" class={button} onclick={() => wizard.removeAddons(index)}>
                    Remove<span class="sr-only"> addons folder {index + 1}</span>
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
          <button type="button" class="{button} self-start" onclick={() => wizard.addAddons()}>Add a folder</button>
        {:else if wizard.stepId === 'python'}
          <fieldset class="flex flex-col gap-2">
            <legend class="mb-2 text-sm font-medium">Python for the project's virtual environment</legend>
            {#if wizard.pythons.status === 'loading' || wizard.pythons.status === 'idle'}
              <p class="text-sm text-fg-muted" role="status">Looking for Python interpreters…</p>
            {:else if wizard.pythons.status === 'error'}
              <p class="text-sm text-state-crashed" role="alert">{wizard.pythons.message}</p>
            {:else}
              {@const list = wizard.pythons.value}
              {@const recommended = list.interpreters.find((i) => i.path === list.recommended)}
              <label class={option}>
                <input
                  type="radio"
                  name="python"
                  class="mt-1 accent-accent"
                  checked={wizard.python === null}
                  onchange={() => (wizard.python = null)}
                />
                <span class="min-w-0">
                  <span class="block text-sm font-medium">Automatic <span class="font-normal text-fg-muted">(recommended)</span></span>
                  <span class="block text-sm break-all text-fg-muted">
                    {#if recommended}Python {recommended.version} · {recommended.path}{:else}{list.problem ?? 'No suitable interpreter found.'}{/if}
                  </span>
                </span>
              </label>
              {#each list.interpreters as python (python.path)}
                {@const usable = python.compat !== 'too_old'}
                <label class={option}>
                  <input
                    type="radio"
                    name="python"
                    class="mt-1 accent-accent"
                    disabled={!usable}
                    checked={wizard.python === python.path}
                    onchange={() => (wizard.python = python.path)}
                  />
                  <span class="min-w-0">
                    <span class="block text-sm font-medium">
                      Python {python.version}
                      {#if !python.standard}<span class="font-normal text-fg-muted">· {python.free_threaded ? 'free-threaded' : python.implementation}</span>{/if}
                      {#if python.compat === 'too_old'}<span class="font-normal text-state-crashed">· too old for this Odoo</span>
                      {:else if python.compat === 'too_new'}<span class="font-normal text-state-starting">· newer than Odoo supports</span>{/if}
                    </span>
                    <span class="block text-sm break-all text-fg-muted">{python.path}</span>
                  </span>
                </label>
              {/each}
              <label class={option}>
                <input type="radio" name="python" class="mt-1 accent-accent" checked={otherPython} onchange={chooseOtherPython} />
                <span class="flex min-w-0 flex-1 flex-col gap-1.5">
                  <span class="text-sm font-medium">Another interpreter</span>
                  <input
                    class={input}
                    aria-label="Interpreter path"
                    bind:value={customPython}
                    oninput={chooseOtherPython}
                    placeholder="/usr/bin/python3.12"
                    spellcheck="false"
                  />
                </span>
              </label>
              {#if list.support}
                <p class="text-sm text-fg-muted">This Odoo supports Python {list.support.min.join('.')}–{list.support.max.join('.')}.</p>
              {/if}
            {/if}
          </fieldset>
          <fieldset class="flex flex-col gap-2">
            <legend class="sr-only">Setup</legend>
            <label class="flex items-center gap-2 text-sm">
              <input type="checkbox" class="accent-accent" bind:checked={wizard.setupNow} />
              Create the virtual environment after creating the project
            </label>
            <label class="ml-6 flex items-center gap-2 text-sm has-disabled:opacity-50">
              <input type="checkbox" class="accent-accent" bind:checked={wizard.installRequirements} disabled={!wizard.setupNow} />
              Install the requirements of Odoo and the modules
            </label>
          </fieldset>
        {:else if wizard.stepId === 'instance'}
          <div class="grid max-w-xl gap-4 sm:grid-cols-2">
            <div class="flex flex-col gap-1.5">
              <label for="wiz-instance" class="text-sm font-medium">Instance id</label>
              <input id="wiz-instance" class={input} bind:value={wizard.instanceId} autocomplete="off" spellcheck="false" />
            </div>
            <div class="flex flex-col gap-1.5">
              <label for="wiz-port" class="text-sm font-medium">HTTP port</label>
              <input
                id="wiz-port"
                class={input}
                bind:value={wizard.httpPort}
                inputmode="numeric"
                placeholder="auto"
                aria-describedby="wiz-port-help"
              />
            </div>
            <div class="flex flex-col gap-1.5 sm:col-span-2">
              <label for="wiz-db" class="text-sm font-medium">Database</label>
              <input id="wiz-db" class={input} bind:value={wizard.db} placeholder={wizard.defaultDb} spellcheck="false" />
            </div>
          </div>
          <p id="wiz-port-help" class="text-sm text-fg-muted">
            Leave the port empty to use 8069, or the next free port when it is taken. A port you enter is kept: the start
            fails while something else uses it.
          </p>

          <dl class="grid gap-x-4 gap-y-1 rounded-md bg-surface-sunken p-3 text-sm sm:grid-cols-[auto_1fr]">
            <dt class="text-fg-muted">Project</dt>
            <dd class="break-all">{wizard.name}</dd>
            <dt class="text-fg-muted">Odoo</dt>
            <dd class="break-all">{wizard.communitySource?.odoo_version} · {wizard.community}{wizard.enterprise ? ` + ${wizard.enterprise}` : ''}</dd>
            <dt class="text-fg-muted">Addons</dt>
            <dd class="break-all">addons/{wizard.addons.filter((a) => a !== '').map((a) => `, ${a}`).join('')}</dd>
            <dt class="text-fg-muted">Python</dt>
            <dd class="break-all">
              {wizard.python ?? 'automatic'}{wizard.setupNow ? (wizard.installRequirements ? ' · set up with requirements' : ' · set up without requirements') : ' · set up later'}
            </dd>
          </dl>
        {/if}

        {#if tried && problem}
          <p class="text-sm text-state-crashed" role="alert">{problem}</p>
        {/if}
        {#if wizard.createError}
          <p class="text-sm text-state-crashed" role="alert">Could not create the project: {wizard.createError}</p>
        {/if}

        <div class="mt-2 flex flex-wrap items-center gap-2 border-t border-line pt-4">
          {#if wizard.step > 0}
            <button type="button" class={button} onclick={() => wizard.back()} disabled={phase.name === 'creating'}>Back</button>
          {:else}
            <a href="/" class={button} onclick={router.link}>Cancel</a>
          {/if}
          <span class="flex-1"></span>
          {#if wizard.isLast}
            <button type="submit" class={primary} disabled={phase.name === 'creating'}>
              {phase.name === 'creating' ? 'Creating…' : 'Create project'}
            </button>
          {:else}
            <button type="submit" class={primary}>Next</button>
          {/if}
        </div>
      </form>
    {:else if phase.name === 'setting_up' || phase.name === 'setup_failed'}
      <h2 id="wizard-heading" bind:this={heading} tabindex="-1" class="text-lg font-semibold">
        {phase.name === 'setting_up' ? `Setting up ${phase.project.name}` : `${phase.project.name} was created, but its setup did not finish`}
      </h2>
      <p class="mt-1 text-sm text-fg-muted">
        {phase.name === 'setting_up'
          ? 'Creating the virtual environment and installing requirements. This can take a few minutes.'
          : 'You can try again, or open the project and set it up later.'}
      </p>
      <div class="mt-4">
        {#if setupJob}
          <SetupProgress job={setupJob} install={wizard.installRequirements} />
        {:else if phase.name === 'setting_up'}
          <p class="text-sm text-fg-muted" role="status">Starting…</p>
        {/if}
      </div>
      {#if phase.name === 'setup_failed'}
        <p class="mt-4 text-sm break-words text-state-crashed" role="alert">{phase.message}</p>
      {/if}
      <div class="mt-6 flex flex-wrap gap-2 border-t border-line pt-4">
        {#if phase.name === 'setting_up'}
          <button type="button" class={button} onclick={() => wizard.cancelSetup()}>Cancel setup</button>
        {:else}
          <button type="button" class={primary} onclick={() => wizard.setup(phase.project)}>Try again</button>
          <button type="button" class={button} onclick={() => openProject(phase.project.name)}>Open project</button>
        {/if}
      </div>
    {:else if phase.name === 'done'}
      <h2 id="wizard-heading" bind:this={heading} tabindex="-1" class="text-lg font-semibold">{phase.project.name} is ready</h2>
      <p class="mt-1 text-sm text-fg-muted">
        {phase.job !== null
          ? 'Its Python environment is set up. Start its instance from the dashboard or the project page.'
          : 'Set up its Python environment before starting it.'}
      </p>
      {#if setupJob}
        <div class="mt-4"><SetupProgress job={setupJob} install={wizard.installRequirements} /></div>
      {/if}
      <div class="mt-6 flex flex-wrap gap-2 border-t border-line pt-4">
        <button type="button" class={primary} onclick={() => openProject(phase.project.name)}>Open project</button>
        <a href="/" class={button} onclick={router.link}>Dashboard</a>
      </div>
    {/if}
  </section>
</div>
