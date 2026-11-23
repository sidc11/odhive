// Start, stop and restart from the UI. The card shows the outcome through
// the live state; failures become toasts.
import { ApiRequestError, type ApiClient } from './api/client'
import { instanceKey, type ProjectsStore } from './projects.svelte'
import type { ToastStore } from './toasts.svelte'

export type InstanceAction = 'start' | 'stop' | 'restart'

const VERB: Record<InstanceAction, string> = { start: 'start', stop: 'stop', restart: 'restart' }

export class InstanceActions {
  /** Action under way per `instanceKey`. */
  private pending = $state<Record<string, InstanceAction>>({})
  private readonly api: ApiClient
  private readonly projects: ProjectsStore
  private readonly toasts: ToastStore

  constructor(api: ApiClient, projects: ProjectsStore, toasts: ToastStore) {
    this.api = api
    this.projects = projects
    this.toasts = toasts
  }

  busy(project: string, instance: string): InstanceAction | undefined {
    return this.pending[instanceKey(project, instance)]
  }

  /** Runs `action`; a second request for the same instance meanwhile is ignored. */
  async run(action: InstanceAction, project: string, instance: string): Promise<void> {
    const key = instanceKey(project, instance)
    if (this.pending[key]) return
    this.pending[key] = action
    // A retry replaces the toast of the previous failure.
    this.toasts.dismissKey(`action:${key}`)
    try {
      if (action === 'stop') {
        const result = await this.api.stopInstance(project, instance)
        if (result.killed) {
          this.toasts.push({
            kind: 'warning',
            title: `${key} was killed`,
            body: 'It did not stop in time.',
            key: `action:${key}`,
          })
        }
        // An instance run outside this service is not in the live state.
        if (!result.status) void this.projects.load()
      } else {
        const info =
          action === 'start'
            ? await this.api.startInstance(project, instance)
            : await this.api.restartInstance(project, instance)
        this.projects.upsert(info)
      }
    } catch (error) {
      this.toasts.push({
        kind: 'error',
        title: `Could not ${VERB[action]} ${key}`,
        body: error instanceof ApiRequestError ? error.message : String(error),
        key: `action:${key}`,
      })
      // Our view was out of date (e.g. it was started elsewhere).
      if (error instanceof ApiRequestError && error.status === 409) void this.projects.load()
    } finally {
      // `$state` records are proxies; delete removes the key reactively.
      // eslint-disable-next-line @typescript-eslint/no-dynamic-delete
      delete this.pending[key]
    }
  }
}
