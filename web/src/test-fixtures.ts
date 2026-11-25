// Shapes the API sends, for component and store tests.
import type {
  ClientMessage,
  EnvCheck,
  EnvCheckReport,
  InstanceInfo,
  InstanceView,
  OdooOption,
  Project,
  ProjectOptions,
  SchemaInfo,
  ServerMessage,
  Status,
} from './lib/api'
import type { SocketLike } from './lib/live.svelte'

export function instanceView(id: string, extra: Partial<InstanceView> = {}): InstanceView {
  return { id, db: `db_${id}`, workers: 0, reload_mode: 'auto', autofix_mode: 'ask', ...extra }
}

export function live(project: string, instance: string, status: Status): InstanceInfo {
  return {
    project,
    instance,
    http_port: 8069,
    gevent_port: 8072,
    db: `db_${instance}`,
    supervised: true,
    port_retried: false,
    port_changes: [],
    status,
  }
}

export function project(name: string, instances: InstanceView[] = [], extra: Partial<Project> = {}): Project {
  return {
    name,
    path: `/home/dev/OdHive/projects/${name}`,
    addons_paths: [],
    git: [],
    has_venv: true,
    odoo_version: '19.0',
    instances,
    ...extra,
  }
}

/** A `fetch` that answers `GET /api/v1/projects` with `body` and `status`. */
export function projectsFetch(body: unknown, status = 200): typeof fetch {
  return async () =>
    new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
}

/**
 * A `fetch` answering `"METHOD /path"` (path below the API prefix, with
 * query) from `routes`; anything else gets 404. Calls are recorded.
 */
export function routedFetch(routes: Record<string, [number, unknown?]>) {
  const calls: { method: string; path: string; headers: Record<string, string>; body: unknown }[] = []
  const fetcher: typeof fetch = async (input, init = {}) => {
    const method = init.method ?? 'GET'
    const path = String(input).replace(/^\/api\/v1/, '')
    const headers = (init.headers ?? {}) as Record<string, string>
    calls.push({ method, path, headers, body: init.body ? JSON.parse(String(init.body)) : undefined })
    const route = routes[`${method} ${path}`]
    if (!route) return new Response(JSON.stringify({ error: 'not_found', message: 'no route' }), { status: 404 })
    const [status, body] = route
    return new Response(body === undefined ? null : JSON.stringify(body), { status })
  }
  return { fetcher, calls }
}

/** A WebSocket stand-in the test drives by hand. */
export class FakeSocket implements SocketLike {
  readyState = 0
  sent: ClientMessage[] = []
  closed: number | null = null
  onopen: ((event: Event) => void) | null = null
  onmessage: ((event: MessageEvent) => void) | null = null
  onclose: ((event: CloseEvent) => void) | null = null
  onerror: ((event: Event) => void) | null = null
  readonly url: string

  constructor(url: string) {
    this.url = url
  }

  send(data: string): void {
    this.sent.push(JSON.parse(data) as ClientMessage)
  }

  close(code = 1005): void {
    this.readyState = 3
    this.closed = code
  }

  open(): void {
    this.readyState = 1
    this.onopen?.(new Event('open'))
  }

  receive(message: ServerMessage | string): void {
    const data = typeof message === 'string' ? message : JSON.stringify(message)
    this.onmessage?.(new MessageEvent('message', { data }))
  }

  /** The connection fails or the server goes away. */
  drop(code = 1006): void {
    this.readyState = 3
    this.onerror?.(new Event('error'))
    this.onclose?.(new CloseEvent('close', { code }))
  }
}

/** Creates `FakeSocket`s and remembers them. */
export function fakeSockets() {
  const sockets: FakeSocket[] = []
  return {
    sockets,
    create: (url: string): FakeSocket => {
      const socket = new FakeSocket(url)
      sockets.push(socket)
      return socket
    },
    last: (): FakeSocket => {
      const socket = sockets.at(-1)
      if (!socket) throw new Error('no socket created')
      return socket
    },
  }
}

/** An environment check report; `overrides` replace checks by id. */
export function envReport(overrides: Partial<Record<EnvCheck['id'], Partial<EnvCheck>>> = {}): EnvCheckReport {
  const ids: EnvCheck['id'][] = ['postgresql', 'python', 'wkhtmltopdf', 'git', 'uv']
  return {
    package_manager: 'apt',
    checks: ids.map((id) => ({
      id,
      status: 'ok',
      summary: 'Found',
      version: null,
      location: null,
      details: [],
      fix: null,
      ...overrides[id],
    })),
  }
}

/** An option of the Odoo option schema (a string option in "Common options" unless changed). */
export function odooOption(key: string, extra: Partial<OdooOption> = {}): OdooOption {
  return {
    key,
    type: 'string',
    flags: [`--${key.replace(/_/g, '-')}`],
    default: null,
    help: null,
    group: 'Common options',
    metavar: null,
    file_loadable: true,
    file_exportable: true,
    secret: false,
    ...extra,
  } as OdooOption
}

/** A small option schema: secrets, a choice, an int, a bool, a managed and a command-line only option. */
export const configSchema: SchemaInfo = {
  source: 'introspected',
  schema: {
    series: '19.0',
    options: [
      odooOption('admin_passwd', { group: null, flags: [], secret: true, default: 'admin' }),
      odooOption('db_password', { group: 'Database related options', secret: true }),
      odooOption('db_host', { group: 'Database related options', help: 'specify the database host' }),
      odooOption('log_level', {
        type: 'choice',
        choices: ['info', 'debug', 'warn'],
        default: 'info',
        group: 'Logging Configuration',
      } as Partial<OdooOption>),
      odooOption('limit_time_cpu', { type: 'int', default: 60, group: 'Advanced options' }),
      odooOption('list_db', { type: 'bool', default: true, group: 'Advanced options' }),
      odooOption('http_port', { type: 'int', group: 'HTTP Service Configuration' }),
      odooOption('save', { type: 'bool', file_loadable: false }),
    ],
  },
}

/** Saved options of `configSchema`'s project. */
export function projectOptions(extra: Partial<ProjectOptions> = {}): ProjectOptions {
  return {
    options: { log_level: 'debug', x_module: 'on' },
    secrets_set: ['admin_passwd'],
    secret_keys: ['admin_passwd', 'db_password', 'smtp_password', 'proxy_access_token'],
    managed_keys: ['addons_path', 'data_dir', 'http_port', 'db_name'],
    conf_path: '/home/dev/OdHive/projects/shop/odoo.conf',
    ...extra,
  }
}
