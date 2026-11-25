<script lang="ts">
  import { onMount } from 'svelte'
  import CommandPalette from './lib/palette/CommandPalette.svelte'
  import { router } from './lib/router.svelte'
  import SettingsDrawer from './lib/settings/SettingsDrawer.svelte'
  import AppShell from './lib/shell/AppShell.svelte'
  import Toaster from './lib/shell/Toaster.svelte'
  import { connectStores, toasts } from './lib/stores'
  import ConfigPage from './pages/ConfigPage.svelte'
  import Dashboard from './pages/Dashboard.svelte'
  import Environment from './pages/Environment.svelte'
  import InstancePage from './pages/InstancePage.svelte'
  import LogsPage from './pages/LogsPage.svelte'
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
      case 'environment':
        return 'Environment · OdHive'
      case 'project':
        return `${route.project} · OdHive`
      case 'instance':
        return `${route.project}/${route.instance} · OdHive`
      case 'config':
        return `${route.project} options · OdHive`
      case 'logs':
        return 'Logs · OdHive'
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
  {:else if route.name === 'environment'}
    <Environment />
  {:else if route.name === 'project'}
    <ProjectPage name={route.project} />
  {:else if route.name === 'instance'}
    <InstancePage project={route.project} instance={route.instance} />
  {:else if route.name === 'config'}
    {#key route.project}
      <ConfigPage project={route.project} />
    {/key}
  {:else if route.name === 'logs'}
    <LogsPage panes={route.panes} />
  {:else}
    <NotFound path={route.path} />
  {/if}
</AppShell>

<CommandPalette />
<SettingsDrawer />
<Toaster {toasts} />
