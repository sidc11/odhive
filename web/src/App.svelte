<script lang="ts">
  import { onMount } from 'svelte'
  import { router } from './lib/router.svelte'
  import AppShell from './lib/shell/AppShell.svelte'
  import Toaster from './lib/shell/Toaster.svelte'
  import { connectStores, toasts } from './lib/stores'
  import Dashboard from './pages/Dashboard.svelte'
  import InstancePage from './pages/InstancePage.svelte'
  import NewProject from './pages/NewProject.svelte'
  import NotFound from './pages/NotFound.svelte'
  import ProjectPage from './pages/ProjectPage.svelte'

  const route = $derived(router.route)

  const title = $derived.by(() => {
    switch (route.name) {
      case 'dashboard':
        return 'OdHive'
      case 'new_project':
        return 'New project · OdHive'
      case 'project':
        return `${route.project} · OdHive`
      case 'instance':
        return `${route.project}/${route.instance} · OdHive`
      case 'not_found':
        return 'Not found · OdHive'
    }
  })

  onMount(() => connectStores())
</script>

<svelte:head>
  <title>{title}</title>
</svelte:head>

<AppShell>
  {#if route.name === 'dashboard'}
    <Dashboard />
  {:else if route.name === 'new_project'}
    <NewProject />
  {:else if route.name === 'project'}
    <ProjectPage name={route.project} />
  {:else if route.name === 'instance'}
    <InstancePage project={route.project} instance={route.instance} />
  {:else}
    <NotFound path={route.path} />
  {/if}
</AppShell>

<Toaster {toasts} />
