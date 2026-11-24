<script lang="ts">
  import { untrack } from 'svelte'
  import { api, connection, toasts } from '../stores'
  import LogView from './LogView.svelte'
  import { LogStream, type LogConnection } from './stream.svelte'
  import type { FileLink } from './traceback'

  let {
    project,
    instance,
    live = connection,
  }: { project: string; instance: string; live?: LogConnection } = $props()

  async function open(link: FileLink): Promise<void> {
    try {
      await api.openFile(project, { path: link.path, line: link.line })
    } catch (error) {
      toasts.push({
        kind: 'error',
        title: `Could not open ${link.path.split('/').at(-1) ?? link.path}:${link.line}`,
        body: error instanceof Error ? error.message : String(error),
        key: 'open-file',
      })
    }
  }

  // The parent keys this component by instance, so the props never change.
  const stream = untrack(() => new LogStream(live, project, instance))

  $effect(() => {
    stream.start()
    return () => stream.stop()
  })
</script>

<LogView {stream} label={`Log of ${project}/${instance}`} onopen={(link) => void open(link)} />
