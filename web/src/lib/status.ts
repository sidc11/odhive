import type { InstanceView, Project } from './api'

/** What an instance is doing, as the UI shows it. */
export type InstanceState =
  | 'starting'
  | 'running'
  | 'stopping'
  | 'stopped'
  | 'crashed'
  | 'failed_to_start'
  /** Runs outside this service (e.g. `odhive start` in a terminal). */
  | 'external'

export const STATE_LABELS: Record<InstanceState, string> = {
  starting: 'Starting',
  running: 'Running',
  stopping: 'Stopping',
  stopped: 'Stopped',
  crashed: 'Crashed',
  failed_to_start: 'Failed to start',
  external: 'Running outside OdHive',
}

export function instanceState(view: InstanceView): InstanceState {
  if (view.live) return view.live.status.state
  if (view.external) return 'external'
  return 'stopped'
}

/** Has a process (or is about to): counts as "up" in summaries. */
export function isUp(state: InstanceState): boolean {
  return state === 'starting' || state === 'running' || state === 'stopping' || state === 'external'
}

export function upCount(project: Project): number {
  return project.instances.filter((view) => isUp(instanceState(view))).length
}

/** Which of start, stop and restart make sense now (card and palette agree). */
export interface AllowedActions {
  start: boolean
  stop: boolean
  restart: boolean
}

export function allowedActions(project: Project, view: InstanceView): AllowedActions {
  const state = instanceState(view)
  return {
    start: !isUp(state) && project.has_venv && !project.error,
    stop: state === 'starting' || state === 'running' || state === 'external',
    restart: state === 'starting' || state === 'running',
  }
}
