<script lang="ts">
  import { untrack } from 'svelte'
  import { connection } from '../stores'
  import LogView from './LogView.svelte'
  import { LogStream, type LogConnection } from './stream.svelte'

  let {
    project,
    instance,
    live = connection,
  }: { project: string; instance: string; live?: LogConnection } = $props()

  // The parent keys this component by instance, so the props never change.
  const stream = untrack(() => new LogStream(live, project, instance))

  $effect(() => {
    stream.start()
    return () => stream.stop()
  })
</script>

<LogView {stream} label={`Log of ${project}/${instance}`} />
