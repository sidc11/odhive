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

/** The parsed body of a `status` answer, or the `ApiRequestError` it means. */
function answer<T>(status: number, text: string): T {
  let parsed: unknown = null
  if (text !== '') {
    try {
      parsed = JSON.parse(text)
    } catch {
      parsed = null
    }
  }
  if (status >= 200 && status < 300) return parsed as T
  if (status === 401) {
    throw new ApiRequestError(401, 'unauthorized', 'Not signed in. Run `odhive` in a terminal to open a signed-in tab.')
  }
  if (isErrorBody(parsed)) throw new ApiRequestError(status, parsed.error, parsed.message)
  throw new ApiRequestError(status, 'http_error', `Request failed (HTTP ${status}).`)
}

/** Bytes sent of an upload so far, and its size. */
export interface UploadProgress {
  sent: number
  total: number
}

export interface Upload {
  url: string
  headers: Record<string, string>
  body: Blob
  onprogress?: (progress: UploadProgress) => void
  signal?: AbortSignal
}

/** Sends a raw `POST` body; answers the status and the body text. */
export type Uploader = (upload: Upload) => Promise<{ status: number; text: string }>

/** Error code of an upload the user stopped. */
export const ABORTED = 'aborted'

/** `fetch` cannot report upload progress; `XMLHttpRequest` can. */
export const xhrUpload: Uploader = (upload) =>
  new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest()
    xhr.open('POST', upload.url)
    xhr.withCredentials = true
    for (const [key, value] of Object.entries(upload.headers)) xhr.setRequestHeader(key, value)
    xhr.upload.onprogress = (event) => upload.onprogress?.({ sent: event.loaded, total: event.total })
    xhr.onload = () => resolve({ status: xhr.status, text: xhr.responseText })
    xhr.onerror = () => reject(new ApiRequestError(0, UNREACHABLE, 'OdHive is not reachable. Is the service running?'))
    xhr.onabort = () => reject(new ApiRequestError(0, ABORTED, 'Upload stopped.'))
    if (upload.signal?.aborted) {
      reject(new ApiRequestError(0, ABORTED, 'Upload stopped.'))
      return
    }
    upload.signal?.addEventListener('abort', () => xhr.abort(), { once: true })
    xhr.send(upload.body)
  })

interface Request {
  body?: unknown
  query?: Record<string, string | number | boolean | null | undefined>
}

export class ApiClient {
  private readonly fetcher: typeof fetch
  private readonly uploader: Uploader

  constructor(fetcher: typeof fetch = (...args) => fetch(...args), uploader: Uploader = xhrUpload) {
    this.fetcher = fetcher
    this.uploader = uploader
  }

  private url(path: string, query: Request['query'] = {}): string {
    let url = `${API_PREFIX}${path}`
    const params = new URLSearchParams()
    for (const [key, value] of Object.entries(query)) {
      if (value !== null && value !== undefined) params.set(key, String(value))
    }
    if (params.size > 0) url += `?${params}`
    return url
  }

  private async call<T>(method: 'GET' | 'POST' | 'PUT' | 'DELETE', path: string, request: Request = {}): Promise<T> {
    const url = this.url(path, request.query)
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
    return answer<T>(response.status, text)
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

  previewProjectOptions(name: string, body: Body<'preview_project_options'>): Promise<Ok<'preview_project_options', 200>> {
    return this.post(`/projects/${segment(name)}/options/preview`, body)
  }

  importProjectOptions(name: string, body: Body<'import_project_options'>): Promise<Ok<'import_project_options', 200>> {
    return this.post(`/projects/${segment(name)}/options/import`, body)
  }

  /** The saved value of one secret option (only for the reveal toggle; never cached). */
  revealProjectSecret(name: string, body: Body<'reveal_project_secret'>): Promise<Ok<'reveal_project_secret', 200>> {
    return this.post(`/projects/${segment(name)}/options/reveal`, body)
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

  // Databases

  private projectPath(project: string): string {
    return `/projects/${segment(project)}`
  }

  checkProjectPostgres(project: string): Promise<Ok<'check_project_postgres', 200>> {
    return this.get(`${this.projectPath(project)}/postgres`)
  }

  listDatabases(project: string): Promise<Ok<'list_project_databases', 200>> {
    return this.get(`${this.projectPath(project)}/databases`)
  }

  createDatabase(project: string, body: Body<'create_project_database'>): Promise<Ok<'create_project_database', 202>> {
    return this.post(`${this.projectPath(project)}/databases`, body)
  }

  /** Uploads a dump (Odoo zip, pg_dump archive or SQL script) and starts restoring it. */
  async restoreDatabase(
    project: string,
    file: Blob,
    query: Query<'restore_project_database'>,
    options: { onprogress?: (progress: UploadProgress) => void; signal?: AbortSignal } = {},
  ): Promise<Ok<'restore_project_database', 202>> {
    const { status, text } = await this.uploader({
      url: this.url(`${this.projectPath(project)}/databases/restore`, query),
      headers: { Accept: 'application/json', [MUTATION_HEADER]: '1', 'Content-Type': 'application/octet-stream' },
      body: file,
      ...options,
    })
    return answer(status, text)
  }

  deleteDatabase(
    project: string,
    database: string,
    body: Body<'delete_project_database'>,
  ): Promise<Ok<'delete_project_database', 202>> {
    return this.call('DELETE', `${this.projectPath(project)}/databases/${segment(database)}`, { body })
  }

  duplicateDatabase(
    project: string,
    database: string,
    body: Body<'duplicate_project_database'>,
  ): Promise<Ok<'duplicate_project_database', 202>> {
    return this.post(`${this.projectPath(project)}/databases/${segment(database)}/duplicate`, body)
  }

  backupDatabase(
    project: string,
    database: string,
    body: Body<'backup_project_database'>,
  ): Promise<Ok<'backup_project_database', 202>> {
    return this.post(`${this.projectPath(project)}/databases/${segment(database)}/backup`, body)
  }

  snapshotDatabase(
    project: string,
    database: string,
    body: Body<'snapshot_project_database'>,
  ): Promise<Ok<'snapshot_project_database', 202>> {
    return this.post(`${this.projectPath(project)}/databases/${segment(database)}/snapshot`, body)
  }

  // Backups

  listBackups(project: string): Promise<Ok<'list_project_backups', 200>> {
    return this.get(`${this.projectPath(project)}/backups`)
  }

  /** Where the browser downloads a backup (a plain link; the cookie signs it in). */
  backupUrl(project: string, file: string): string {
    return this.url(`${this.projectPath(project)}/backups/${segment(file)}`)
  }

  deleteBackup(project: string, file: string): Promise<void> {
    return this.call('DELETE', `${this.projectPath(project)}/backups/${segment(file)}`)
  }

  // Snapshots

  listSnapshots(project: string): Promise<Ok<'list_project_snapshots', 200>> {
    return this.get(`${this.projectPath(project)}/snapshots`)
  }

  restoreSnapshot(
    project: string,
    snapshot: string,
    body: Body<'restore_project_snapshot'>,
  ): Promise<Ok<'restore_project_snapshot', 202>> {
    return this.post(`${this.projectPath(project)}/snapshots/${segment(snapshot)}/restore`, body)
  }

  deleteSnapshot(project: string, snapshot: string): Promise<void> {
    return this.call('DELETE', `${this.projectPath(project)}/snapshots/${segment(snapshot)}`)
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
