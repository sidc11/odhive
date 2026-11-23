import { describe, expect, it } from 'vitest'
import type { Job, JobStep } from '../api'
import { cloneSummary, setupStages } from './progress'

function job(steps: JobStep[], extra: Partial<Job> = {}): Job {
  return { id: 1, kind: 'setup', project: 'shop', state: 'running', started_at: 1, steps, output: [], output_dropped: 0, ...extra } as Job
}

const PYTHON: JobStep = {
  job: 'setup',
  step: 'python',
  path: '/usr/bin/python3.14',
  version: '3.14.0',
  compat: 'supported',
  support: { min: [3, 10], max: [3, 14] },
}
const VENV_START: JobStep = { job: 'setup', step: 'venv', event: 'started', dir: '/p/.venv', python: '/usr/bin/python3.14', tool: { kind: 'stdlib' } }
const VENV_DONE: JobStep = { job: 'setup', step: 'venv', event: 'finished', python: '/p/.venv/bin/python', version: '3.14.0' }
const COLLECTED: JobStep = { job: 'setup', step: 'collected', count: 42, modules: 1, skipped: [], warnings: [] }

const states = (j: Job, install = true) => setupStages(j, install).map((s) => s.state)

describe('setupStages', () => {
  it('follows the steps of a setup', () => {
    expect(states(job([]))).toEqual(['active', 'pending', 'pending'])
    expect(states(job([PYTHON]))).toEqual(['done', 'active', 'pending'])
    expect(states(job([PYTHON, VENV_START]))).toEqual(['done', 'active', 'pending'])
    expect(states(job([PYTHON, VENV_START, VENV_DONE]))).toEqual(['done', 'done', 'active'])
    const collecting = setupStages(job([PYTHON, VENV_START, VENV_DONE, COLLECTED]))
    expect(collecting.map((s) => s.state)).toEqual(['done', 'done', 'active'])
    expect(collecting[0]?.detail).toBe('Python 3.14.0 · /usr/bin/python3.14')
    expect(collecting[2]?.detail).toBe('42 requirements from 1 module')
    expect(states(job([PYTHON, VENV_START, VENV_DONE, COLLECTED], { state: 'succeeded' }))).toEqual(['done', 'done', 'done'])
  })

  it('marks where it failed, a kept venv and skipped requirements', () => {
    expect(states(job([PYTHON, VENV_START], { state: 'failed' }))).toEqual(['done', 'failed', 'pending'])
    const kept = setupStages(job([{ job: 'setup', step: 'venv_kept', dir: '/p/.venv' }], { state: 'succeeded' }), false)
    expect(kept.map((s) => s.state)).toEqual(['skipped', 'done', 'skipped'])
    expect(kept[1]?.detail).toMatch(/Kept/)
  })

  it('warns about a too-new Python and skipped requirements', () => {
    const stages = setupStages(
      job([
        { ...PYTHON, compat: 'too_new' } as JobStep,
        { ...COLLECTED, skipped: [{ text: '-e .', reason: 'option', origin: { kind: 'file', line: 3, path: '/src/requirements.txt' } }] } as JobStep,
      ]),
    )
    expect(stages[0]?.warning).toMatch(/3\.10–3\.14/)
    expect(stages[2]?.warning).toMatch(/1 entry/)
  })
})

describe('cloneSummary', () => {
  it('shows git progress and the end', () => {
    const clone = (steps: JobStep[], extra: Partial<Job> = {}) => job(steps, { kind: 'clone', version: '19.0', ...extra } as Partial<Job>)
    expect(cloneSummary(clone([]))).toBe('Starting…')
    expect(cloneSummary(clone([{ job: 'clone', event: 'started', branch: '19.0', target: '/t', url: 'u' }]))).toBe('Cloning 19.0…')
    expect(cloneSummary(clone([{ job: 'clone', event: 'progress', phase: 'Receiving objects', percent: 40 }]))).toBe('Receiving objects 40%')
    expect(cloneSummary(clone([{ job: 'clone', event: 'progress', phase: 'Resolving deltas', percent: null }]))).toBe('Resolving deltas')
    expect(cloneSummary(clone([], { state: 'failed', error: { error: 'clone_failed', message: 'no network' } }))).toBe('no network')
    expect(cloneSummary(clone([], { state: 'succeeded' }))).toMatch(/Cloned/)
  })
})
