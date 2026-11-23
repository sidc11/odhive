// Background jobs (source clones, project setups), kept current over the
// WebSocket (`subscribe_jobs`).
import type { Job, JobEvent, ServerMessage } from './api'

/** Output lines kept per job in the UI (the server keeps its own tail). */
export const MAX_OUTPUT_LINES = 500

/** A job this store no longer knows, e.g. after the service restarted. */
export class JobLostError extends Error {
  constructor(id: number) {
    super(`Job ${id} is gone; the OdHive service may have restarted.`)
    this.name = 'JobLostError'
  }
}

interface Waiter {
  resolve: (job: Job) => void
  reject: (error: Error) => void
}

export class JobsStore {
  /** Jobs, oldest first. */
  jobs = $state<Job[]>([])

  // Callbacks, not state: nothing renders them.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  private waiters = new Map<number, Waiter[]>()

  get(id: number): Job | undefined {
    return this.jobs.find((job) => job.id === id)
  }

  /**
   * Resolves with job `id` once it has ended (succeeded, failed or
   * cancelled); rejects with `JobLostError` when a snapshot no longer lists
   * it.
   */
  finished(id: number): Promise<Job> {
    const known = this.get(id)
    if (known && known.state !== 'running') return Promise.resolve(known)
    return new Promise((resolve, reject) => {
      const list = this.waiters.get(id) ?? []
      list.push({ resolve, reject })
      this.waiters.set(id, list)
    })
  }

  private settle(snapshot = false): void {
    for (const [id, list] of this.waiters) {
      const job = this.get(id)
      if (job && job.state === 'running') continue
      if (!job && !snapshot) continue
      this.waiters.delete(id)
      for (const waiter of list) {
        if (job) waiter.resolve($state.snapshot(job) as Job)
        else waiter.reject(new JobLostError(id))
      }
    }
  }

  /** Adds or replaces a job, e.g. the `202` answer that started it. */
  upsert(job: Job): void {
    const index = this.jobs.findIndex((candidate) => candidate.id === job.id)
    if (index === -1) {
      this.jobs.push(job)
      this.jobs.sort((a, b) => a.id - b.id)
    } else {
      this.jobs[index] = job
    }
    this.settle()
  }

  apply(message: ServerMessage): void {
    if (message.type === 'job_snapshot') {
      this.jobs = [...message.jobs].sort((a, b) => a.id - b.id)
      this.settle(true)
    } else if (message.type === 'job_events') {
      for (const event of message.events) this.applyEvent(event)
    }
  }

  private applyEvent(event: JobEvent): void {
    switch (event.event) {
      case 'started':
      case 'finished':
        this.upsert(event.job)
        break
      case 'step':
        this.get(event.id)?.steps.push(event.step)
        break
      case 'output': {
        const job = this.get(event.id)
        if (!job) break
        job.output.push(event.line)
        const extra = job.output.length - MAX_OUTPUT_LINES
        if (extra > 0) {
          job.output.splice(0, extra)
          job.output_dropped += extra
        }
        break
      }
    }
  }
}
