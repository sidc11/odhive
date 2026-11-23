// The environment check (`GET /environment`): PostgreSQL, Python,
// wkhtmltopdf, git and uv, each with a fix when something is off.
import type { EnvCheck, EnvCheckId, EnvCheckReport, EnvStatus } from './api'
import type { ApiClient } from './api/client'

export const CHECK_LABELS: Record<EnvCheckId, string> = {
  postgresql: 'PostgreSQL',
  python: 'Python',
  wkhtmltopdf: 'wkhtmltopdf',
  git: 'git',
  uv: 'uv',
}

/** What each check is needed for. */
export const CHECK_PURPOSES: Record<EnvCheckId, string> = {
  postgresql: 'Odoo stores every database in it.',
  python: 'Each Odoo series runs on a range of Python versions.',
  wkhtmltopdf: 'Odoo prints PDF reports with it.',
  git: 'Needed to clone Odoo.',
  uv: 'Optional: faster virtual environments and installs.',
}

export const STATUS_LABELS: Record<EnvStatus, string> = {
  ok: 'OK',
  info: 'Optional',
  warning: 'Warning',
  error: 'Problem',
}

const RANK: Record<EnvStatus, number> = { ok: 0, info: 1, warning: 2, error: 3 }

/** The worst status of `checks` (`ok` when empty). */
export function worstStatus(checks: readonly EnvCheck[]): EnvStatus {
  return checks.reduce<EnvStatus>((worst, c) => (RANK[c.status] > RANK[worst] ? c.status : worst), 'ok')
}

/** Checks Odoo cannot run without fixing. */
export function blockers(report: EnvCheckReport): EnvCheck[] {
  return report.checks.filter((c) => c.status === 'error')
}

export type EnvState =
  | { status: 'idle' }
  | { status: 'loading'; previous: EnvCheckReport | null }
  | { status: 'ready'; report: EnvCheckReport; checkedAt: number }
  | { status: 'error'; message: string; previous: EnvCheckReport | null }

export class EnvironmentStore {
  state = $state<EnvState>({ status: 'idle' })
  private readonly api: Pick<ApiClient, 'checkEnvironment'>
  private readonly now: () => number
  private run = 0

  constructor(api: Pick<ApiClient, 'checkEnvironment'>, now: () => number = Date.now) {
    this.api = api
    this.now = now
  }

  /** The last report, also while checking again or after a failed re-check. */
  get report(): EnvCheckReport | null {
    const s = this.state
    if (s.status === 'ready') return s.report
    if (s.status === 'idle') return null
    return s.previous
  }

  /** Runs the check (again); a newer run wins over an older one. */
  async check(): Promise<void> {
    const run = ++this.run
    const previous = this.report
    this.state = { status: 'loading', previous }
    try {
      const report = await this.api.checkEnvironment()
      if (run === this.run) this.state = { status: 'ready', report, checkedAt: this.now() }
    } catch (error) {
      if (run === this.run) {
        const message = error instanceof Error ? error.message : String(error)
        this.state = { status: 'error', message, previous }
      }
    }
  }

  /** Runs the check unless it has run or is running. */
  ensure(): Promise<void> {
    return this.state.status === 'idle' ? this.check() : Promise.resolve()
  }
}
