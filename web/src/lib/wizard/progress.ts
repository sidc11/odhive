// Turns a job's steps into a short checklist for the UI.
import type { Job } from '../api'

export type StageState = 'pending' | 'active' | 'done' | 'failed' | 'skipped'

export interface Stage {
  id: 'python' | 'venv' | 'requirements'
  label: string
  state: StageState
  /** One line about what happened, e.g. the Python version. */
  detail?: string
  /** A problem worth showing while the stage itself went on. */
  warning?: string
}

const ORDER: Stage['id'][] = ['python', 'venv', 'requirements']

const LABELS: Record<Stage['id'], string> = {
  python: 'Pick Python',
  venv: 'Create the virtual environment',
  requirements: 'Install requirements',
}

function plural(count: number, word: string): string {
  return `${count} ${word}${count === 1 ? '' : 's'}`
}

/** Checklist of a project setup job; `install` = requirements were asked for. */
export function setupStages(job: Job, install = true): Stage[] {
  const stages = new Map<Stage['id'], Stage>(
    ORDER.map((id) => [id, { id, label: LABELS[id], state: 'pending' as StageState }]),
  )
  const stage = (id: Stage['id']): Stage => stages.get(id) as Stage
  const activate = (id: Stage['id']): void => {
    // Everything before an active stage has finished.
    for (const before of ORDER.slice(0, ORDER.indexOf(id))) {
      const s = stage(before)
      if (s.state === 'pending' || s.state === 'active') s.state = 'done'
    }
    stage(id).state = 'active'
  }

  for (const step of job.steps) {
    if (step.job !== 'setup') continue
    switch (step.step) {
      case 'python': {
        const s = stage('python')
        s.state = 'done'
        s.detail = `Python ${step.version} · ${step.path}`
        const range = `${step.support.min.join('.')}–${step.support.max.join('.')}`
        if (step.compat === 'too_new') s.warning = `Newer than Odoo supports (${range}); some packages may not build.`
        break
      }
      case 'venv_kept':
        stage('python').state = 'skipped'
        activate('venv')
        stage('venv').state = 'done'
        stage('venv').detail = 'Kept the existing environment'
        break
      case 'venv':
        if (step.event === 'finished') {
          activate('venv')
          stage('venv').state = 'done'
          stage('venv').detail = `Python ${step.version}`
        } else if (step.event !== 'output') {
          activate('venv')
        }
        break
      case 'collected': {
        activate('requirements')
        const s = stage('requirements')
        s.detail = `${plural(step.count, 'requirement')} from ${plural(step.modules, 'module')}`
        const notes = step.skipped.length + step.warnings.length
        if (notes > 0) s.warning = `${plural(notes, 'entry')} skipped or with problems; see the output.`
        break
      }
      case 'install':
        activate('requirements')
        if (step.event === 'finished') stage('requirements').state = 'done'
        break
    }
  }

  if (!install && stage('requirements').state === 'pending') stage('requirements').state = 'skipped'
  if (job.state === 'succeeded') {
    for (const s of stages.values()) if (s.state === 'pending' || s.state === 'active') s.state = 'done'
  } else if (job.state === 'failed' || job.state === 'cancelled') {
    const failed = ORDER.map(stage).find((s) => s.state === 'active') ?? ORDER.map(stage).find((s) => s.state === 'pending')
    if (failed) failed.state = 'failed'
  } else if (!ORDER.some((id) => stage(id).state === 'active')) {
    const next = ORDER.map(stage).find((s) => s.state === 'pending')
    if (next) next.state = 'active'
  }
  return ORDER.map(stage)
}

/** One line about a running clone: git's phase and percentage. */
export function cloneSummary(job: Job): string {
  if (job.state === 'succeeded') return 'Cloned and registered'
  if (job.state === 'cancelled') return 'Cancelled'
  if (job.state === 'failed') return job.error?.message ?? 'Clone failed'
  for (let i = job.steps.length - 1; i >= 0; i--) {
    const step = job.steps[i]
    if (step?.job !== 'clone') continue
    if (step.event === 'progress') return step.percent == null ? step.phase : `${step.phase} ${step.percent}%`
    if (step.event === 'started') return `Cloning ${step.branch}…`
    if (step.event === 'finished') return 'Verifying…'
  }
  return 'Starting…'
}
