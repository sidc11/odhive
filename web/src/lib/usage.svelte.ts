// CPU, memory and uptime of running instances, polled from
// `GET /instances/usage` while a page that shows them is open and the tab is
// visible.
import type { InstanceUsage } from './api'
import type { ApiClient } from './api/client'
import { instanceKey } from './projects.svelte'

/** Between two usage requests (the server samples at most once a second). */
export const USAGE_POLL_MS = 3000
/** Uptime clock tick. */
export const CLOCK_TICK_MS = 1000

export class UsageStore {
  /** Latest sample per `instanceKey`. */
  private byKey = $state<Record<string, InstanceUsage>>({})
  /** Wall clock in milliseconds, ticking while polling (for uptimes). */
  now = $state(Date.now())
  private users = 0
  private poller: ReturnType<typeof setInterval> | null = null
  private clock: ReturnType<typeof setInterval> | null = null
  private inFlight = false
  private readonly api: ApiClient

  constructor(api: ApiClient) {
    this.api = api
  }

  get(project: string, instance: string): InstanceUsage | undefined {
    return this.byKey[instanceKey(project, instance)]
  }

  /**
   * Starts polling (shared between callers); returns the function that
   * stops it for this caller.
   */
  start(): () => void {
    this.users += 1
    if (this.users === 1) {
      void this.refresh()
      this.poller = setInterval(() => void this.refresh(), USAGE_POLL_MS)
      this.clock = setInterval(() => (this.now = Date.now()), CLOCK_TICK_MS)
    }
    let stopped = false
    return () => {
      if (stopped) return
      stopped = true
      this.users -= 1
      if (this.users === 0) {
        if (this.poller !== null) clearInterval(this.poller)
        if (this.clock !== null) clearInterval(this.clock)
        this.poller = null
        this.clock = null
      }
    }
  }

  /** Fetches a sample now, unless the tab is hidden or one is under way. */
  async refresh(): Promise<void> {
    if (this.inFlight || document.visibilityState === 'hidden') return
    this.inFlight = true
    try {
      const sample = await this.api.instanceUsage()
      const byKey: Record<string, InstanceUsage> = {}
      for (const usage of sample) byKey[instanceKey(usage.project, usage.instance)] = usage
      this.byKey = byKey
      this.now = Date.now()
    } catch {
      // Keep the last sample; the top bar already says when OdHive is
      // unreachable.
    } finally {
      this.inFlight = false
    }
  }
}
