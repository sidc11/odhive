// Turns WebSocket events into toasts: crashes, port switches, auto-fix
// progress (with Install / Not now for packages that need a yes), Smart
// Reload problems and finished jobs. Routine events (running, reloads,
// output) stay quiet.
import type { ExitInfo, Hint, Job, ManagerEvent, ServerMessage } from './api'
import { ApiRequestError, type ApiClient } from './api/client'
import type { ToastInput, ToastStore } from './toasts.svelte'

/** Longest body text shown in a toast. */
export const MAX_BODY = 400

function clip(text: string): string {
  const trimmed = text.trim()
  return trimmed.length > MAX_BODY ? `${trimmed.slice(0, MAX_BODY - 1)}…` : trimmed
}

function exitText(exit: ExitInfo): string {
  const how =
    exit.signal !== null && exit.signal !== undefined
      ? `Killed by signal ${exit.signal}.`
      : `Exit code ${exit.code ?? 'unknown'}.`
  const last = exit.tail.trim().split('\n').at(-1)
  return clip(last ? `${how} ${last}` : how)
}

export function hintText(hint: Hint): string {
  const commands = hint.command ? [hint.command] : hint.commands.map((c) => `${c.manager}: ${c.command}`)
  return clip([...commands, hint.note ?? ''].filter((line) => line !== '').join('\n'))
}

function jobName(job: Job): string {
  return job.kind === 'clone' ? `Clone of Odoo ${job.version}` : `Setup of ${job.project}`
}

export function jobToast(job: Job): ToastInput | null {
  const key = `job:${job.id}`
  switch (job.state) {
    case 'succeeded':
      return { kind: 'success', key, title: `${jobName(job)} finished` }
    case 'failed':
      return { kind: 'error', key, title: `${jobName(job)} failed`, body: clip(job.error?.message ?? '') || undefined }
    case 'cancelled':
      return { kind: 'info', key, title: `${jobName(job)} cancelled` }
    case 'running':
      return null
  }
}

export class Notifier {
  private readonly toasts: ToastStore
  private readonly api: ApiClient

  constructor(toasts: ToastStore, api: ApiClient) {
    this.toasts = toasts
    this.api = api
  }

  handle(message: ServerMessage): void {
    if (message.type === 'events') {
      for (const event of message.events) this.event(event)
    } else if (message.type === 'job_events') {
      for (const event of message.events) {
        if (event.event !== 'finished') continue
        const toast = jobToast(event.job)
        if (toast) this.toasts.push(toast)
      }
    }
  }

  private event(event: ManagerEvent): void {
    const name = `${event.project}/${event.instance}`
    // One toast per topic and instance; a newer one replaces it.
    const stateKey = `${name}:state`
    const fixKey = `${name}:fix`
    const push = (toast: ToastInput): void => {
      this.toasts.push(toast)
    }

    switch (event.event) {
      case 'status':
        switch (event.status.state) {
          case 'crashed':
            push({ kind: 'error', key: stateKey, title: `${name} crashed`, body: exitText(event.status.exit) })
            break
          case 'failed_to_start':
            push({ kind: 'error', key: stateKey, title: `${name} failed to start`, body: clip(event.status.error) })
            break
          case 'running':
          case 'stopped':
            // Back up (or stopped on purpose): an old crash toast is moot.
            this.toasts.dismissKey(stateKey)
            break
          default:
            break
        }
        break
      case 'spawned':
        for (const change of event.changes) {
          push({
            kind: 'info',
            key: `${name}:port:${change.kind}`,
            title: `${name}: port ${change.requested} busy`,
            body: `Using ${change.kind === 'http' ? 'HTTP' : 'gevent'} port ${change.assigned} instead.`,
          })
        }
        break
      case 'port_retry':
        push({
          kind: 'warning',
          key: stateKey,
          title: `${name}: ports taken during start`,
          body: `Ports ${event.http_port}/${event.gevent_port} were grabbed by another program; retrying on new ports.`,
        })
        break
      case 'dependency_missing':
        this.missing(event, name, fixKey)
        break
      case 'dependency_installing':
        push({
          kind: 'info',
          key: fixKey,
          title: `${name}: installing ${event.package}`,
          body: `For missing module “${event.module}” (attempt ${event.attempt} of ${event.max}).`,
        })
        break
      case 'dependency_installed':
        push({ kind: 'success', key: fixKey, title: `${name}: installed ${event.package}`, body: 'Restarting.' })
        break
      case 'dependency_failed':
        push({
          kind: 'error',
          key: fixKey,
          title: `${name}: installing ${event.package} failed`,
          body: clip(event.hint ? hintText(event.hint) : event.error),
        })
        break
      case 'system_library_missing':
        push({
          kind: 'warning',
          key: `${name}:syslib:${event.hint.library}`,
          title: `${name}: a system library is missing (${event.hint.library})`,
          body: hintText(event.hint),
          timeout: null,
        })
        break
      case 'reload_failed':
        push({ kind: 'warning', key: `${name}:reload`, title: `${name}: Smart Reload stopped`, body: clip(event.error) })
        break
      case 'module_added':
        push({
          kind: 'info',
          title: `${name}: new module “${event.module}”`,
          body: 'Install it from Odoo’s Apps menu.',
        })
        break
      case 'log_file':
        push({ kind: 'warning', key: `${name}:log`, title: `${name}: log file not written`, body: clip(event.error) })
        break
      default:
        break
    }
  }

  private missing(event: Extract<ManagerEvent, { event: 'dependency_missing' }>, name: string, key: string): void {
    const what = event.package ? `${event.package} (module “${event.module}”)` : `module “${event.module}”`
    switch (event.action.kind) {
      case 'install':
        // `dependency_installing` follows at once.
        break
      case 'confirm':
        this.toasts.push({
          kind: 'warning',
          key,
          title: `${name}: install ${what}?`,
          body: 'It is not in Odoo’s requirements or a module manifest. Install it into the project venv?',
          actions: [
            { label: 'Install', run: () => this.fix(event.project, event.instance, key, true) },
            { label: 'Not now', run: () => this.fix(event.project, event.instance, key, false) },
          ],
        })
        break
      case 'report':
        this.toasts.push({
          kind: 'warning',
          key,
          title: `${name}: missing Python ${what}`,
          body: 'Auto-fix is off for this instance; install it into the project venv.',
        })
        break
      case 'give_up': {
        const reason = event.action.reason
        const why =
          reason.reason === 'limit'
            ? `Already installed ${reason.max} packages for this start.`
            : reason.reason === 'still_missing'
              ? `${reason.package} was installed but the module is still missing.`
              : reason.refusal === 'part_of_odoo'
                ? 'The module is part of Odoo itself; check the addons path.'
                : 'The name is not a valid package name.'
        this.toasts.push({ kind: 'error', key, title: `${name}: missing Python ${what}`, body: why })
        break
      }
    }
  }

  private async fix(project: string, instance: string, key: string, install: boolean): Promise<void> {
    this.toasts.dismissKey(key)
    try {
      if (install) await this.api.confirmFix(project, instance)
      else await this.api.declineFix(project, instance)
    } catch (error) {
      this.toasts.push({
        kind: 'error',
        key,
        title: `${project}/${instance}: ${install ? 'install' : 'decline'} failed`,
        body: error instanceof ApiRequestError ? error.message : undefined,
      })
    }
  }
}
