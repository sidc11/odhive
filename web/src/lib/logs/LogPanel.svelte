<script lang="ts">
  import { untrack } from 'svelte'
  import { api, logStreams, toasts } from '../stores'
  import LogView from './LogView.svelte'
  import type { StreamPool } from './pool'
  import type { FileLink } from './traceback'

  let {
    project,
    instance,
    pool = logStreams,
  }: { project: string; instance: string; pool?: StreamPool } = $props()

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
  const stream = untrack(() => pool.acquire(project, instance))

  $effect(() => () => pool.release(stream))
</script>

<LogView {stream} label={`Log of ${project}/${instance}`} source={{ project, instance }} onopen={(link) => void open(link)} />
