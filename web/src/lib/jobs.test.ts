import { describe, expect, it } from 'vitest'
import type { Job } from './api'
import { JobLostError, JobsStore, MAX_OUTPUT_LINES } from './jobs.svelte'

function job(id: number, extra: Partial<Job> = {}): Job {
  return {
    id,
    kind: 'setup',
    project: 'shop',
    state: 'running',
    started_at: 1_790_000_000_000,
    steps: [],
    output: [],
    output_dropped: 0,
    ...extra,
  } as Job
}

describe('JobsStore', () => {
  it('takes a snapshot, sorted by id', () => {
    const jobs = new JobsStore()
    jobs.apply({ type: 'job_snapshot', jobs: [job(2), job(1)] })
    expect(jobs.jobs.map((j) => j.id)).toEqual([1, 2])
  })

  it('follows started, step, output and finished events', () => {
    const jobs = new JobsStore()
    jobs.apply({ type: 'job_events', events: [{ event: 'started', job: job(3) }] })
    jobs.apply({
      type: 'job_events',
      events: [
        { event: 'step', id: 3, step: { job: 'setup', step: 'venv', event: 'output', line: 'Using CPython 3.14.0' } },
        { event: 'output', id: 3, line: 'Installed 42 packages' },
        { event: 'output', id: 99, line: 'unknown job: ignored' },
      ],
    })
    expect(jobs.get(3)?.steps).toHaveLength(1)
    expect(jobs.get(3)?.output).toEqual(['Installed 42 packages'])

    jobs.apply({ type: 'job_events', events: [{ event: 'finished', job: job(3, { state: 'succeeded', output: ['done'] }) }] })
    expect(jobs.get(3)).toMatchObject({ state: 'succeeded', output: ['done'] })
    expect(jobs.jobs).toHaveLength(1)
  })

  it('keeps a bounded output tail and counts what it dropped', () => {
    const jobs = new JobsStore()
    jobs.upsert(job(1))
    const events = Array.from({ length: MAX_OUTPUT_LINES + 3 }, (_, i) => ({ event: 'output' as const, id: 1, line: `line ${i}` }))
    jobs.apply({ type: 'job_events', events })
    const kept = jobs.get(1)
    expect(kept?.output).toHaveLength(MAX_OUTPUT_LINES)
    expect(kept?.output[0]).toBe('line 3')
    expect(kept?.output_dropped).toBe(3)
  })

  it('resolves finished() once the job ends, also when it already has', async () => {
    const jobs = new JobsStore()
    jobs.upsert(job(1))
    const waiting = jobs.finished(1)
    jobs.apply({ type: 'job_events', events: [{ event: 'finished', job: job(1, { state: 'failed' }) }] })
    await expect(waiting).resolves.toMatchObject({ id: 1, state: 'failed' })
    await expect(jobs.finished(1)).resolves.toMatchObject({ state: 'failed' })

    // A job not seen yet (its 202 answer still on the way) is waited for.
    const later = jobs.finished(2)
    jobs.upsert(job(2, { state: 'succeeded' }))
    await expect(later).resolves.toMatchObject({ id: 2, state: 'succeeded' })
  })

  it('rejects finished() when a snapshot no longer lists the job', async () => {
    const jobs = new JobsStore()
    jobs.upsert(job(1))
    const waiting = jobs.finished(1)
    jobs.apply({ type: 'job_snapshot', jobs: [job(1)] })
    jobs.apply({ type: 'job_snapshot', jobs: [] })
    await expect(waiting).rejects.toBeInstanceOf(JobLostError)
  })
})
