<script lang="ts">
  import { shortcutLabel } from '../palette/palette.svelte'
  import { routeHref, router, type Route } from '../router.svelte'
  import { connection, palette } from '../stores'
  import ConnectionStatus from './ConnectionStatus.svelte'
  import ThemeToggle from './ThemeToggle.svelte'

  let {
    drawerOpen,
    onmenu,
    menuButton = $bindable(),
  }: { drawerOpen: boolean; onmenu: () => void; menuButton?: HTMLButtonElement } = $props()

  type Crumb = { label: string; href?: string }

  function crumbs(route: Route): Crumb[] {
    switch (route.name) {
      case 'dashboard':
        return [{ label: 'Dashboard' }]
      case 'new_project':
        return [{ label: 'New project' }]
      case 'environment':
        return [{ label: 'Environment' }]
      case 'project':
        return [{ label: route.project }]
      case 'instance':
        return [
          { label: route.project, href: routeHref({ name: 'project', project: route.project }) },
          { label: route.instance },
        ]
      case 'config':
        return [
          { label: route.project, href: routeHref({ name: 'project', project: route.project }) },
          { label: 'Odoo options' },
        ]
      case 'logs':
        return [{ label: 'Logs' }]
      case 'not_found':
        return [{ label: 'Not found' }]
    }
  }

  const trail = $derived(crumbs(router.route))
  const shortcut = shortcutLabel()
</script>

<header class="sticky top-0 z-30 flex h-14 items-center gap-2 border-b border-line bg-surface-raised px-2 md:px-4">
  <button
    bind:this={menuButton}
    type="button"
    class="inline-flex size-9 items-center justify-center rounded-md text-fg-muted hover:bg-surface-sunken hover:text-fg md:hidden"
    aria-label="Open navigation"
    aria-expanded={drawerOpen}
    aria-controls="nav-drawer"
    onclick={onmenu}
  >
    <svg viewBox="0 0 24 24" class="size-5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
      <path d="M4 6h16M4 12h16M4 18h16" />
    </svg>
  </button>

  <nav aria-label="Breadcrumb" class="min-w-0 flex-1">
    <ol class="flex min-w-0 items-center gap-1.5 text-sm">
      {#each trail as crumb, index (index)}
        {#if index > 0}
          <li aria-hidden="true" class="text-fg-muted">/</li>
        {/if}
        <li class="min-w-0 truncate {index === trail.length - 1 ? 'font-semibold' : 'text-fg-muted'}">
          {#if crumb.href}
            <a href={crumb.href} class="hover:text-fg hover:underline" onclick={router.link}>{crumb.label}</a>
          {:else}
            <span aria-current="page">{crumb.label}</span>
          {/if}
        </li>
      {/each}
    </ol>
  </nav>

  <ConnectionStatus {connection} />
  <button
    type="button"
    class="inline-flex size-9 items-center justify-center gap-2 rounded-md text-sm text-fg-muted hover:bg-surface-sunken hover:text-fg sm:w-auto sm:border sm:border-line sm:px-2.5"
    aria-haspopup="dialog"
    aria-keyshortcuts="Control+K Meta+K"
    title="Command palette ({shortcut})"
    onclick={() => palette.show()}
  >
    <svg viewBox="0 0 24 24" class="size-4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"
      ><circle cx="11" cy="11" r="6.5" /><path d="m16 16 4 4" /></svg
    >
    <!-- The name includes the visible shortcut (WCAG 2.5.3, label in name). -->
    <span class="sr-only">Command palette</span>
    <kbd class="hidden font-sans text-xs sm:inline">{shortcut}</kbd>
  </button>
  <ThemeToggle />
</header>
