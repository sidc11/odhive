<script lang="ts">
  import { onMount } from 'svelte'
  import { projects } from './lib/projects.svelte'
  import { router } from './lib/router.svelte'
  import AppShell from './lib/shell/AppShell.svelte'
  import Dashboard from './pages/Dashboard.svelte'
  import InstancePage from './pages/InstancePage.svelte'
  import NotFound from './pages/NotFound.svelte'
  import ProjectPage from './pages/ProjectPage.svelte'

  const route = $derived(router.route)

  const title = $derived.by(() => {
    switch (route.name) {
      case 'dashboard':
        return 'OdHive'
      case 'project':
        return `${route.project} · OdHive`
      case 'instance':
        return `${route.project}/${route.instance} · OdHive`
      case 'not_found':
        return 'Not found · OdHive'
    }
  })

  onMount(() => {
    void projects.load()
  })
</script>

<svelte:head>
  <title>{title}</title>
</svelte:head>

<AppShell>
  {#if route.name === 'dashboard'}
    <Dashboard />
  {:else if route.name === 'project'}
    <ProjectPage name={route.project} />
  {:else if route.name === 'instance'}
    <InstancePage project={route.project} instance={route.instance} />
  {:else}
    <NotFound path={route.path} />
  {/if}
</AppShell>
