// State and flow of the new-project wizard. The page renders it; all checks
// and API calls live here so they can be tested without a DOM.
import type { CreateProject, Job, Project, PythonList, SourceEntry } from '../api'
import { ApiRequestError, type ApiClient } from '../api/client'
import type { JobsStore } from '../jobs.svelte'
import type { ProjectsStore } from '../projects.svelte'

export const STEPS = ['name', 'source', 'enterprise', 'addons', 'python', 'instance'] as const
export type StepId = (typeof STEPS)[number]

export const STEP_LABELS: Record<StepId, string> = {
  name: 'Name',
  source: 'Odoo source',
  enterprise: 'Enterprise',
  addons: 'Addons',
  python: 'Python',
  instance: 'First instance',
}

/** Series OdHive can clone (Odoo 19 and 20 Community). */
export const CLONE_VERSIONS = ['19.0', '20.0'] as const

// Same rules as the server (odhive-core `names`).
const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,63}$/
const DB_RE = /^[A-Za-z0-9_.-]{1,63}$/
const MIN_PORT = 1024
const MAX_PORT = 65535

export function nameProblem(name: string, taken: readonly string[] = []): string | null {
  if (name === '') return 'Enter a name.'
  if (!NAME_RE.test(name)) {
    return 'Use lowercase letters, digits, - and _ (up to 64), starting with a letter or digit.'
  }
  if (taken.includes(name)) return `A project named “${name}” exists already.`
  return null
}

export function pathProblem(path: string, what = 'a folder path'): string | null {
  if (path === '') return `Enter ${what}.`
  if (!path.startsWith('/')) return 'Use an absolute path (starting with /).'
  return null
}

export function dbProblem(db: string): string | null {
  if (db === '') return null
  return DB_RE.test(db) ? null : 'Use letters, digits, ., - and _ (up to 63).'
}

export function portProblem(text: string): string | null {
  if (text === '') return null
  const port = Number(text)
  if (!/^\d+$/.test(text) || port < MIN_PORT || port > MAX_PORT) {
    return `Enter a port from ${MIN_PORT} to ${MAX_PORT}, or leave it empty.`
  }
  return null
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

/** The step a create error belongs to. */
export function stepOfError(code: string): StepId {
  switch (code) {
    case 'invalid_name':
    case 'exists':
      return 'name'
    case 'unknown_source':
      return 'source'
    case 'bad_addons_path':
      return 'addons'
    case 'bad_path':
      return 'python'
    default:
      return 'instance'
  }
}

export type Phase =
  /** Filling in the steps. */
  | { name: 'editing' }
  | { name: 'creating' }
  /** The project exists; its environment is being set up by job `job`. */
  | { name: 'setting_up'; project: Project; job: number }
  /** Created; `job` = the setup that ran, if any. */
  | { name: 'done'; project: Project; job: number | null }
  /** The project exists but its setup did not finish. */
  | { name: 'setup_failed'; project: Project; job: number | null; message: string }

type Loadable<T> = { status: 'idle' } | { status: 'loading' } | { status: 'ready'; value: T } | { status: 'error'; message: string }

export class ProjectWizard {
  step = $state(0)
  /** Highest step reached, so finished steps can be revisited. */
  reached = $state(0)

  name = $state('')
  community = $state<string | null>(null)
  enterprise = $state<string | null>(null)
  addons = $state<string[]>([])
  /** Interpreter path; `null` = the one setup picks. */
  python = $state<string | null>(null)
  setupNow = $state(true)
  installRequirements = $state(true)
  instanceId = $state('main')
  db = $state('')
  httpPort = $state('')

  sources = $state<Loadable<SourceEntry[]>>({ status: 'idle' })
  pythons = $state<Loadable<PythonList>>({ status: 'idle' })
  private pythonsFor: string | null = null

  /** Running clone job, if any. */
  cloneJob = $state<number | null>(null)
  /** Problem with the last source registration or clone. */
  sourceError = $state<string | null>(null)
  registering = $state(false)

  phase = $state<Phase>({ name: 'editing' })
  /** Why the last create failed, shown on `step`. */
  createError = $state<string | null>(null)

  private readonly api: ApiClient
  private readonly jobs: JobsStore
  private readonly projects: ProjectsStore

  constructor(api: ApiClient, jobs: JobsStore, projects: ProjectsStore) {
    this.api = api
    this.jobs = jobs
    this.projects = projects
  }

  /** A job this wizard started, as the jobs store has it now. */
  job(id: number | null): Job | undefined {
    return id === null ? undefined : this.jobs.get(id)
  }

  get stepId(): StepId {
    return STEPS[this.step] ?? 'name'
  }

  get takenNames(): string[] {
    return this.projects.state.status === 'ready' ? this.projects.state.projects.map((p) => p.name) : []
  }

  get communitySources(): SourceEntry[] {
    return this.sources.status === 'ready' ? this.sources.value.filter((s) => s.edition === 'community') : []
  }

  get communitySource(): SourceEntry | undefined {
    return this.communitySources.find((s) => s.id === this.community)
  }

  /** Enterprise trees of the chosen Community series. */
  get enterpriseSources(): SourceEntry[] {
    const series = this.communitySource?.odoo_version
    if (this.sources.status !== 'ready' || !series) return []
    return this.sources.value.filter((s) => s.edition === 'enterprise' && s.odoo_version === series)
  }

  get defaultDb(): string {
    return `${this.name || '<name>'}_${this.instanceId || 'main'}`
  }

  /** What keeps step `id` from being finished, or null. */
  problem(id: StepId = this.stepId): string | null {
    switch (id) {
      case 'name':
        return nameProblem(this.name, this.takenNames)
      case 'source':
        if (this.cloneJob !== null) return 'Wait for the clone to finish.'
        return this.communitySource ? null : 'Choose an Odoo source.'
      case 'enterprise':
        return this.enterprise === null || this.enterpriseSources.some((s) => s.id === this.enterprise)
          ? null
          : 'Choose an Enterprise tree of the same Odoo series, or none.'
      case 'addons':
        // Empty rows are left out of the request.
        return (
          this.addons
            .filter((path) => path !== '')
            .map((path) => pathProblem(path))
            .find((p) => p !== null) ?? null
        )
      case 'python':
        return this.python === null ? null : pathProblem(this.python, 'the interpreter path')
      case 'instance':
        if (this.instanceId === '') return 'Enter an instance id.'
        if (!NAME_RE.test(this.instanceId)) return 'Use lowercase letters, digits, - and _ for the instance id.'
        return dbProblem(this.db) ?? portProblem(this.httpPort)
    }
  }

  get isLast(): boolean {
    return this.step === STEPS.length - 1
  }

  /** Goes to the next step when this one is finished; returns whether it moved. */
  next(): boolean {
    if (this.problem() !== null || this.isLast) return false
    this.goto(this.step + 1, true)
    return true
  }

  back(): void {
    if (this.step > 0) this.goto(this.step - 1)
  }

  /** Jumps to a step already reached (or the next one when `forward`). */
  goto(index: number, forward = false): void {
    if (index < 0 || index >= STEPS.length) return
    if (index > this.reached && !forward) return
    this.step = index
    this.reached = Math.max(this.reached, index)
    if (STEPS[index] === 'source' && this.sources.status === 'idle') void this.loadSources()
    if (STEPS[index] === 'python') void this.loadPythons()
  }

  async loadSources(): Promise<void> {
    this.sources = { status: 'loading' }
    try {
      const value = await this.api.listSources()
      this.sources = { status: 'ready', value }
      if (this.community === null) {
        // Newest series first is the likely choice.
        const newest = [...this.communitySources].sort((a, b) => b.odoo_version.localeCompare(a.odoo_version))[0]
        if (newest) this.chooseCommunity(newest.id)
      }
    } catch (error) {
      this.sources = { status: 'error', message: message(error) }
    }
  }

  chooseCommunity(id: string): void {
    this.community = id
    // An Enterprise tree of another series no longer fits.
    if (this.enterprise !== null && !this.enterpriseSources.some((s) => s.id === this.enterprise)) {
      this.enterprise = null
    }
  }

  private addSource(entry: SourceEntry): void {
    if (this.sources.status !== 'ready') return
    if (!this.sources.value.some((s) => s.id === entry.id)) this.sources.value.push(entry)
  }

  /** Registers an existing folder: Community, or Enterprise of the chosen series. */
  async register(path: string, edition: 'community' | 'enterprise'): Promise<boolean> {
    const problem = pathProblem(path)
    if (problem) {
      this.sourceError = problem
      return false
    }
    const series = this.communitySource?.odoo_version
    if (edition === 'enterprise' && !series) {
      this.sourceError = 'Choose the Odoo source first.'
      return false
    }
    this.registering = true
    this.sourceError = null
    try {
      const entry = await this.api.addSource(edition === 'enterprise' ? { path, enterprise_version: series } : { path })
      this.addSource(entry)
      if (edition === 'enterprise') this.enterprise = entry.id
      else this.chooseCommunity(entry.id)
      return true
    } catch (error) {
      this.sourceError = message(error)
      return false
    } finally {
      this.registering = false
    }
  }

  /** Clones Community `version` into the workspace and chooses it. */
  async clone(version: string): Promise<boolean> {
    if (this.cloneJob !== null) return false
    this.sourceError = null
    let job: Job
    try {
      job = await this.api.cloneSource({ version })
    } catch (error) {
      this.sourceError = message(error)
      return false
    }
    this.jobs.upsert(job)
    this.cloneJob = job.id
    try {
      const done = await this.jobs.finished(job.id)
      if (done.state === 'succeeded' && done.result?.kind === 'clone') {
        this.addSource(done.result.source)
        this.chooseCommunity(done.result.source.id)
        return true
      }
      this.sourceError = done.state === 'cancelled' ? 'The clone was cancelled.' : (done.error?.message ?? 'The clone failed.')
      return false
    } catch (error) {
      this.sourceError = message(error)
      return false
    } finally {
      this.cloneJob = null
    }
  }

  async cancelClone(): Promise<void> {
    if (this.cloneJob === null) return
    try {
      await this.api.cancelJob(this.cloneJob)
    } catch (error) {
      this.sourceError = message(error)
    }
  }

  async loadPythons(): Promise<void> {
    const source = this.community
    if (source === null || (this.pythonsFor === source && this.pythons.status === 'ready')) return
    this.pythonsFor = source
    this.pythons = { status: 'loading' }
    try {
      const value = await this.api.listPythons(source)
      if (this.pythonsFor === source) this.pythons = { status: 'ready', value }
    } catch (error) {
      if (this.pythonsFor === source) this.pythons = { status: 'error', message: message(error) }
    }
  }

  addAddons(path = ''): void {
    this.addons.push(path)
  }

  removeAddons(index: number): void {
    this.addons.splice(index, 1)
  }

  /** The create request from what was entered. */
  request(): CreateProject {
    const addons = this.addons.filter((path) => path !== '')
    return {
      name: this.name,
      community: this.community ?? '',
      enterprise: this.enterprise,
      addons_paths: addons,
      python: this.python,
      instance: {
        id: this.instanceId,
        db: this.db === '' ? null : this.db,
        http_port: this.httpPort === '' ? null : Number(this.httpPort),
      },
    }
  }

  /** Creates the project, then sets up its environment when asked to. */
  async create(): Promise<void> {
    if (this.phase.name !== 'editing') return
    const blocked = STEPS.findIndex((id) => this.problem(id) !== null)
    if (blocked !== -1) {
      this.goto(blocked, true)
      return
    }
    this.phase = { name: 'creating' }
    this.createError = null
    let project: Project
    try {
      project = await this.api.createProject(this.request())
    } catch (error) {
      this.phase = { name: 'editing' }
      this.createError = message(error)
      if (error instanceof ApiRequestError) this.goto(STEPS.indexOf(stepOfError(error.code)), true)
      return
    }
    void this.projects.load()
    if (!this.setupNow) {
      this.phase = { name: 'done', project, job: null }
      return
    }
    await this.setup(project)
  }

  /** Sets up `project`'s environment (again after a failure). */
  async setup(project: Project): Promise<void> {
    let job: Job
    try {
      job = await this.api.setupProject(project.name, { skip_requirements: !this.installRequirements })
    } catch (error) {
      this.phase = { name: 'setup_failed', project, job: null, message: message(error) }
      return
    }
    this.jobs.upsert(job)
    this.phase = { name: 'setting_up', project, job: job.id }
    try {
      const done = await this.jobs.finished(job.id)
      if (done.state === 'succeeded') {
        this.phase = { name: 'done', project, job: job.id }
        void this.projects.load()
      } else {
        const why = done.state === 'cancelled' ? 'The setup was cancelled.' : (done.error?.message ?? 'The setup failed.')
        this.phase = { name: 'setup_failed', project, job: job.id, message: why }
      }
    } catch (error) {
      this.phase = { name: 'setup_failed', project, job: job.id, message: message(error) }
    }
  }

  async cancelSetup(): Promise<void> {
    if (this.phase.name !== 'setting_up') return
    try {
      await this.api.cancelJob(this.phase.job)
    } catch {
      // The job may have just ended; its end arrives as an event.
    }
  }
}
