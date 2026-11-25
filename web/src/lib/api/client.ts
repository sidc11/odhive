// Typed REST client. Request and response types come from the generated
// `operations` (schema.d.ts), so a server change that breaks the UI fails
// `pnpm check`.
import { API_PREFIX, MUTATION_HEADER, type ApiError } from './index'
import type { operations } from './schema'

type OperationId = keyof operations

/** JSON body of the `code` response of `op`. */
type Ok<Op extends OperationId, Code extends number> = operations[Op]['responses'] extends Record<
  Code,
  { content: { 'application/json': infer T } }
>
  ? T
  : never

/** JSON request body of `op`. */
type Body<Op extends OperationId> = operations[Op] extends {
  requestBody: { content: { 'application/json': infer T } }
}
  ? T
  : never

/** Query parameters of `op`. */
type Query<Op extends OperationId> = operations[Op]['parameters'] extends { query?: infer Q } ? NonNullable<Q> : never

/** Error code of a request that never got an answer. */
export const UNREACHABLE = 'unreachable'

/** A request that failed: the server's `{error, message}` or a network error. */
export class ApiRequestError extends Error {
  /** HTTP status; 0 when the server could not be reached. */
  readonly status: number
  /** Stable error code from the server (e.g. `unknown_project`), or `unreachable`. */
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiRequestError'
    this.status = status
    this.code = code
  }

  get unauthorized(): boolean {
    return this.status === 401
  }
}

function isErrorBody(body: unknown): body is ApiError {
  return (
    typeof body === 'object' &&
    body !== null &&
    typeof (body as ApiError).error === 'string' &&
    typeof (body as ApiError).message === 'string'
  )
}

const segment = encodeURIComponent

interface Request {
  body?: unknown
  query?: Record<string, string | number | null | undefined>
}

export class ApiClient {
  private readonly fetcher: typeof fetch

  constructor(fetcher: typeof fetch = (...args) => fetch(...args)) {
    this.fetcher = fetcher
  }

  private async call<T>(method: 'GET' | 'POST' | 'PUT' | 'DELETE', path: string, request: Request = {}): Promise<T> {
    let url = `${API_PREFIX}${path}`
    const params = new URLSearchParams()
    for (const [key, value] of Object.entries(request.query ?? {})) {
      if (value !== null && value !== undefined) params.set(key, String(value))
    }
    if (params.size > 0) url += `?${params}`

    const headers: Record<string, string> = { Accept: 'application/json' }
    if (method !== 'GET') headers[MUTATION_HEADER] = '1'
    let body: string | undefined
    if (request.body !== undefined) {
      headers['Content-Type'] = 'application/json'
      body = JSON.stringify(request.body)
    }

    let response: globalThis.Response
    try {
      response = await this.fetcher(url, { method, headers, body, credentials: 'same-origin' })
    } catch {
      throw new ApiRequestError(0, UNREACHABLE, 'OdHive is not reachable. Is the service running?')
    }
    const text = await response.text().catch(() => '')
    let parsed: unknown = null
    if (text !== '') {
      try {
        parsed = JSON.parse(text)
      } catch {
        parsed = null
      }
    }
    if (response.ok) return parsed as T
    if (response.status === 401) {
      throw new ApiRequestError(401, 'unauthorized', 'Not signed in. Run `odhive` in a terminal to open a signed-in tab.')
    }
    if (isErrorBody(parsed)) throw new ApiRequestError(response.status, parsed.error, parsed.message)
    throw new ApiRequestError(response.status, 'http_error', `Request failed (HTTP ${response.status}).`)
  }

  private get<T>(path: string, query?: Request['query']): Promise<T> {
    return this.call<T>('GET', path, { query })
  }

  private post<T>(path: string, body?: unknown): Promise<T> {
    return this.call<T>('POST', path, { body })
  }

  private instancePath(project: string, instance: string): string {
    return `/projects/${segment(project)}/instances/${segment(instance)}`
  }

  health(): Promise<Ok<'health', 200>> {
    return this.get('/health')
  }

  quit(): Promise<void> {
    return this.post('/service/quit')
  }

  // Sources

  listSources(): Promise<Ok<'list_sources', 200>> {
    return this.get('/sources')
  }

  addSource(body: Body<'add_source'>): Promise<Ok<'add_source', 201>> {
    return this.post('/sources', body)
  }

  cloneSource(body: Body<'clone_source'>): Promise<Ok<'clone_source', 202>> {
    return this.post('/sources/clone', body)
  }

  removeSource(id: string): Promise<Ok<'remove_source', 200>> {
    return this.call('DELETE', `/sources/${segment(id)}`)
  }

  sourceSchema(id: string): Promise<Ok<'source_schema', 200>> {
    return this.get(`/sources/${segment(id)}/schema`)
  }

  // Python

  listPythons(source?: string): Promise<Ok<'list_pythons', 200>> {
    return this.get('/pythons', { source })
  }

  // Environment

  checkEnvironment(): Promise<Ok<'check_environment', 200>> {
    return this.get('/environment')
  }

  // Projects

  listProjects(): Promise<Ok<'list_projects', 200>> {
    return this.get('/projects')
  }

  getProject(name: string): Promise<Ok<'get_project', 200>> {
    return this.get(`/projects/${segment(name)}`)
  }

  createProject(body: Body<'create_project'>): Promise<Ok<'create_project', 201>> {
    return this.post('/projects', body)
  }

  removeProject(name: string): Promise<void> {
    return this.call('DELETE', `/projects/${segment(name)}`)
  }

  setupProject(name: string, body: Body<'setup_project'>): Promise<Ok<'setup_project', 202>> {
    return this.post(`/projects/${segment(name)}/setup`, body)
  }

  /** Opens a project file at a line in the user's editor. */
  openFile(project: string, body: Body<'open_file'>): Promise<void> {
    return this.post(`/projects/${segment(project)}/open-file`, body)
  }

  projectSchema(name: string): Promise<Ok<'project_schema', 200>> {
    return this.get(`/projects/${segment(name)}/schema`)
  }

  getProjectOptions(name: string): Promise<Ok<'get_project_options', 200>> {
    return this.get(`/projects/${segment(name)}/options`)
  }

  putProjectOptions(name: string, body: Body<'put_project_options'>): Promise<Ok<'put_project_options', 200>> {
    return this.call('PUT', `/projects/${segment(name)}/options`, { body })
  }

  // Instances

  listInstances(): Promise<Ok<'list_instances', 200>> {
    return this.get('/instances')
  }

  instanceUsage(): Promise<Ok<'instance_usage', 200>> {
    return this.get('/instances/usage')
  }

  startInstance(project: string, instance: string): Promise<Ok<'start_instance', 200>> {
    return this.post(`${this.instancePath(project, instance)}/start`)
  }

  stopInstance(project: string, instance: string): Promise<Ok<'stop_instance', 200>> {
    return this.post(`${this.instancePath(project, instance)}/stop`)
  }

  restartInstance(project: string, instance: string): Promise<Ok<'restart_instance', 200>> {
    return this.post(`${this.instancePath(project, instance)}/restart`)
  }

  confirmFix(project: string, instance: string): Promise<Ok<'confirm_fix', 200>> {
    return this.post(`${this.instancePath(project, instance)}/fix/confirm`)
  }

  declineFix(project: string, instance: string): Promise<Ok<'decline_fix', 200>> {
    return this.post(`${this.instancePath(project, instance)}/fix/decline`)
  }

  getInstanceSettings(project: string, instance: string): Promise<Ok<'get_instance_settings', 200>> {
    return this.get(`${this.instancePath(project, instance)}/settings`)
  }

  putInstanceSettings(
    project: string,
    instance: string,
    body: Body<'put_instance_settings'>,
  ): Promise<Ok<'put_instance_settings', 200>> {
    return this.call('PUT', `${this.instancePath(project, instance)}/settings`, { body })
  }

  instanceLogs(
    project: string,
    instance: string,
    query: Query<'instance_logs'> = {},
  ): Promise<Ok<'instance_logs', 200>> {
    return this.get(`${this.instancePath(project, instance)}/logs`, query)
  }

  // Jobs

  listJobs(): Promise<Ok<'list_jobs', 200>> {
    return this.get('/jobs')
  }

  getJob(id: number): Promise<Ok<'get_job', 200>> {
    return this.get(`/jobs/${id}`)
  }

  cancelJob(id: number): Promise<Ok<'cancel_job', 200>> {
    return this.post(`/jobs/${id}/cancel`)
  }
}
