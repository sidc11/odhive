// The app's shared state, and the wiring between the WebSocket and the
// stores.
import { InstanceActions } from './actions.svelte'
import { ApiClient } from './api/client'
import { EnvironmentStore } from './environment.svelte'
import { JobsStore } from './jobs.svelte'
import { LiveConnection } from './live.svelte'
import { StreamPool } from './logs/pool'
import { Notifier } from './notices'
import { PaletteStore } from './palette/palette.svelte'
import { ProjectsStore } from './projects.svelte'
import { SettingsDrawerStore } from './settings/settings.svelte'
import { ToastStore } from './toasts.svelte'
import { UsageStore } from './usage.svelte'

export const api = new ApiClient()
export const toasts = new ToastStore()
export const projects = new ProjectsStore(api)
export const jobs = new JobsStore()
export const connection = new LiveConnection()
/** Live logs, one stream per instance shared by every view of it. */
export const logStreams = new StreamPool(connection)
export const usage = new UsageStore(api)
export const actions = new InstanceActions(api, projects, toasts)
export const environment = new EnvironmentStore(api)
export const palette = new PaletteStore()
export const settingsDrawer = new SettingsDrawerStore()

/** Wait after a stale event before asking for a snapshot (events come in bursts). */
export const RESYNC_DELAY_MS = 200

export interface Stores {
  connection: LiveConnection
  projects: ProjectsStore
  jobs: JobsStore
  notifier: Pick<Notifier, 'handle'>
}

/**
 * Loads the projects, opens the WebSocket and keeps the stores current.
 * Returns the function that disconnects.
 */
export function connectStores(stores: Stores = defaultStores()): () => void {
  const { connection, projects, jobs, notifier } = stores
  let opened = false
  let resync: ReturnType<typeof setTimeout> | null = null

  const offOpen = connection.onopen(() => {
    connection.send({ op: 'subscribe_events' })
    connection.send({ op: 'subscribe_jobs' })
    // Anything may have changed while disconnected (projects added, …).
    if (opened) void projects.load()
    opened = true
  })
  const offMessage = connection.onmessage((message) => {
    projects.apply(message)
    jobs.apply(message)
    notifier.handle(message)
  })
  // Subscribing again answers with a fresh snapshot.
  projects.onstale = () => {
    resync ??= setTimeout(() => {
      resync = null
      connection.send({ op: 'subscribe_events' })
    }, RESYNC_DELAY_MS)
  }
  const onvisible = (): void => {
    if (document.visibilityState === 'visible') connection.retryNow()
  }
  document.addEventListener('visibilitychange', onvisible)

  void projects.load()
  connection.start()

  return () => {
    offOpen()
    offMessage()
    projects.onstale = () => {}
    if (resync !== null) clearTimeout(resync)
    document.removeEventListener('visibilitychange', onvisible)
    connection.stop()
  }
}

function defaultStores(): Stores {
  return { connection, projects, jobs, notifier: new Notifier(toasts, api) }
}
