// Background jobs (source clones, project setups), kept current over the
// WebSocket (`subscribe_jobs`).
import type { Job, JobEvent, ServerMessage } from './api'

/** Output lines kept per job in the UI (the server keeps its own tail). */
export const MAX_OUTPUT_LINES = 500

export class JobsStore {
  /** Jobs, oldest first. */
  jobs = $state<Job[]>([])

  get(id: number): Job | undefined {
    return this.jobs.find((job) => job.id === id)
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
  }

  apply(message: ServerMessage): void {
    if (message.type === 'job_snapshot') {
      this.jobs = [...message.jobs].sort((a, b) => a.id - b.id)
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
