<script lang="ts">
  import { tick, type Snippet } from 'svelte'
  import { router } from '../router.svelte'
  import Sidebar from './Sidebar.svelte'
  import TopBar from './TopBar.svelte'

  let { children }: { children: Snippet } = $props()

  let drawerOpen = $state(false)
  let drawer: HTMLElement | undefined = $state()
  let menuButton: HTMLButtonElement | undefined = $state()

  async function openDrawer(): Promise<void> {
    drawerOpen = true
    await tick()
    drawer?.querySelector<HTMLElement>('a, button')?.focus()
  }

  function closeDrawer(): void {
    if (!drawerOpen) return
    drawerOpen = false
    menuButton?.focus()
  }

  function onkeydown(event: KeyboardEvent): void {
    if (!drawerOpen || !drawer) return
    if (event.key === 'Escape') {
      event.preventDefault()
      closeDrawer()
    } else if (event.key === 'Tab') {
      // Modal drawer: keep keyboard focus inside it.
      const focusable = [...drawer.querySelectorAll<HTMLElement>('a[href], button:not([disabled])')]
      const first = focusable[0]
      const last = focusable[focusable.length - 1]
      if (!first || !last) return
      const active = document.activeElement
      if (event.shiftKey && (active === first || !drawer.contains(active))) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && (active === last || !drawer.contains(active))) {
        event.preventDefault()
        first.focus()
      }
    }
  }

  // Close the drawer when the route changes (e.g. browser back).
  $effect(() => {
    void router.path
    drawerOpen = false
  })

  // Above the breakpoint the drawer is hidden by CSS; also drop its state.
  $effect(() => {
    if (typeof window.matchMedia !== 'function') return
    const wide = window.matchMedia('(min-width: 768px)')
    const onchange = (event: MediaQueryListEvent): void => {
      if (event.matches) drawerOpen = false
    }
    wide.addEventListener('change', onchange)
    return () => wide.removeEventListener('change', onchange)
  })
</script>

<svelte:window {onkeydown} />

<a
  href="#content"
  class="sr-only z-50 rounded-md bg-accent px-3 py-2 text-accent-fg focus:not-sr-only focus:fixed focus:top-2 focus:left-2"
>
  Skip to content
</a>

<div class="min-h-dvh md:grid md:grid-cols-[16rem_minmax(0,1fr)]">
  <aside
    class="sticky top-0 hidden h-dvh border-r border-line bg-surface-raised md:block"
    aria-label="Sidebar"
    data-testid="sidebar"
  >
    <Sidebar />
  </aside>

  {#if drawerOpen}
    <div class="fixed inset-0 z-40 md:hidden">
      <button
        type="button"
        class="absolute inset-0 size-full cursor-default bg-backdrop"
        aria-label="Close navigation"
        tabindex="-1"
        onclick={closeDrawer}
      ></button>
      <div
        bind:this={drawer}
        id="nav-drawer"
        role="dialog"
        aria-modal="true"
        aria-label="Navigation"
        class="absolute inset-y-0 left-0 w-72 max-w-[85vw] border-r border-line bg-surface-raised shadow-xl"
      >
        <Sidebar onnavigate={closeDrawer} />
      </div>
    </div>
  {/if}

  <div class="flex min-w-0 flex-col">
    <TopBar {drawerOpen} onmenu={openDrawer} bind:menuButton />
    <main id="content" tabindex="-1" class="flex-1 p-4 focus:outline-none md:p-6">
      {@render children()}
    </main>
  </div>
</div>
