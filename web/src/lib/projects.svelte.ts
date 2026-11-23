// Projects for the whole UI: the project list from REST, with each
// instance's `live` state kept current by WebSocket events.
import type { InstanceInfo, ManagerEvent, Project, ServerMessage } from './api'
import { ApiRequestError, type ApiClient } from './api/client'

export type ProjectsState =
  | { status: 'loading' }
  | { status: 'ready'; projects: Project[] }
  | { status: 'error'; message: string; unauthorized: boolean }

/** Map key of an instance. Names never contain `/`. */
export function instanceKey(project: string, instance: string): string {
  return `${project}/${instance}`
}

export class ProjectsStore {
  /** Projects, with `live` taken from the newest known state. */
  readonly state: ProjectsState = $derived.by(() => this.merge())

  private loaded = $state<ProjectsState>({ status: 'loading' })
  /** Instances this service runs, by `instanceKey`. */
  private live = $state<Record<string, InstanceInfo>>({})
  /** Bumped on each WebSocket update, so an older REST answer does not win. */
  private liveVersion = 0
  private loading: Promise<void> | null = null
  private queued: Promise<void> | null = null
  private readonly api: ApiClient
  /** Asks the server for a fresh snapshot (set by the live wiring). */
  onstale: () => void = () => {}

  constructor(api: ApiClient) {
    this.api = api
  }

  /**
   * Loads the project list. A call while a request is under way gets one
   * more request after it (the running one may predate a change); further
   * calls share that.
   */
  load(): Promise<void> {
    if (this.loading) {
      this.queued ??= this.loading.then(() => {
        this.queued = null
        return this.load()
      })
      return this.queued
    }
    this.loading = this.fetch().finally(() => {
      this.loading = null
    })
    return this.loading
  }

  private async fetch(): Promise<void> {
    const version = this.liveVersion
    let projects: Project[]
    try {
      projects = await this.api.listProjects()
    } catch (error) {
      const failure =
        error instanceof ApiRequestError
          ? { message: error.message, unauthorized: error.unauthorized }
          : { message: 'Loading projects failed.', unauthorized: false }
      // Keep showing a list we already have when a reload fails, unless the
      // sign-in is gone.
      if (this.loaded.status !== 'ready' || failure.unauthorized) this.loaded = { status: 'error', ...failure }
      return
    }
    if (version === this.liveVersion) {
      const live: Record<string, InstanceInfo> = {}
      for (const project of projects) {
        for (const view of project.instances) {
          if (view.live) live[instanceKey(project.name, view.id)] = view.live
        }
      }
      this.live = live
    }
    this.loaded = { status: 'ready', projects }
  }

  /** Records what a start/restart request returned. */
  upsert(info: InstanceInfo): void {
    this.liveVersion += 1
    this.live[instanceKey(info.project, info.instance)] = info
  }

  /** Applies a WebSocket message; ignores the ones not about instances. */
  apply(message: ServerMessage): void {
    if (message.type === 'snapshot') {
      this.liveVersion += 1
      const live: Record<string, InstanceInfo> = {}
      for (const info of message.instances) live[instanceKey(info.project, info.instance)] = info
      this.live = live
    } else if (message.type === 'events') {
      this.liveVersion += 1
      let stale = false
      for (const event of message.events) stale = this.applyEvent(event) || stale
      if (stale) this.onstale()
    }
  }

  /** Returns true when the event needs a snapshot to be fully reflected. */
  private applyEvent(event: ManagerEvent): boolean {
    const key = instanceKey(event.project, event.instance)
    const known = this.live[key]
    switch (event.event) {
      case 'spawned': {
        const info = known ?? this.placeholder(event.project, event.instance)
        this.live[key] = {
          ...info,
          http_port: event.http_port,
          gevent_port: event.gevent_port,
          // Only a first start reports moves; a restart keeps its ports.
          port_changes: event.changes.length > 0 ? event.changes : info.port_changes,
          status: { state: 'starting', pid: event.pid },
        }
        return !known
      }
      case 'status': {
        const info = known ?? this.placeholder(event.project, event.instance)
        this.live[key] = { ...info, status: event.status }
        return !known
      }
      case 'output':
      case 'reload':
      case 'reload_watching':
      case 'log_file':
        return false
      default:
        // Notices (auto-fix, port retry, attach, …) change fields the event
        // does not carry, e.g. the pending fix: take them from a snapshot.
        return true
    }
  }

  /** What is known about an instance before the snapshot arrives. */
  private placeholder(project: string, instance: string): InstanceInfo {
    const view = this.findIn(this.loaded, project)?.instances.find((candidate) => candidate.id === instance)
    return {
      project,
      instance,
      db: view?.db ?? null,
      supervised: true,
      port_retried: false,
      port_changes: [],
      status: { state: 'starting', pid: 0 },
    }
  }

  private merge(): ProjectsState {
    const loaded = this.loaded
    if (loaded.status !== 'ready') return loaded
    const live = this.live
    return {
      status: 'ready',
      projects: loaded.projects.map((project) => ({
        ...project,
        instances: project.instances.map((view) => ({
          ...view,
          live: live[instanceKey(project.name, view.id)] ?? null,
        })),
      })),
    }
  }

  private findIn(state: ProjectsState, name: string): Project | undefined {
    return state.status === 'ready' ? state.projects.find((project) => project.name === name) : undefined
  }

  find(name: string): Project | undefined {
    return this.findIn(this.state, name)
  }
}
