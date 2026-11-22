import { describe, expect, it } from 'vitest'
import document from './openapi.json'
import {
  API_PREFIX,
  WS_PATH,
  type ClientMessage,
  type InstanceInfo,
  type LogRecord,
  type Project,
  type ServerMessage,
} from './index'

describe('API document', () => {
  it('mounts every route below the API prefix', () => {
    const routes = Object.keys(document.paths)
    expect(routes.length).toBeGreaterThan(0)
    for (const route of routes) expect(route.startsWith(`${API_PREFIX}/`)).toBe(true)
  })

  it('gives every operation a unique id', () => {
    const ids = Object.values(document.paths).flatMap((item) =>
      Object.values(item).map((op) => (op as { operationId: string }).operationId),
    )
    expect(new Set(ids).size).toBe(ids.length)
  })
})

// Shapes the server sends (see crates/odhive-server/tests/api.rs); these only
// have to type-check.
const running: InstanceInfo = {
  project: 'shop',
  instance: 'main',
  http_port: 19100,
  gevent_port: 19101,
  db: 'shop_main',
  supervised: true,
  port_retried: false,
  status: { state: 'running', pid: 4242 },
}

const project: Project = {
  name: 'shop',
  path: '/home/dev/OdHive/projects/shop',
  odoo_version: '19.0',
  community: 'odoo-19.0',
  enterprise: null,
  addons_paths: [],
  has_venv: true,
  instances: [
    {
      id: 'main',
      db: 'shop_main',
      http_port: null,
      gevent_port: null,
      workers: 0,
      reload_mode: 'auto',
      autofix_mode: 'auto',
      live: running,
      external: null,
    },
  ],
}

const record: LogRecord = {
  seq: 1,
  timestamp: '2026-11-22T06:28:04.911Z',
  pid: 4242,
  level: 'info',
  db: null,
  logger: 'odoo.service.server',
  message: 'HTTP service (werkzeug) running on 127.0.0.1:19100',
  locations: [],
  exceptions: [],
  truncated: false,
}

// WebSocket messages (see crates/odhive-server/tests/ws.rs).
const subscribe: ClientMessage = { op: 'subscribe_logs', project: 'shop', instance: 'main', tail: 100 }

const frames: ServerMessage[] = [
  { type: 'hello', version: '0.1.0' },
  { type: 'subscribed', project: 'shop', instance: 'main', next: 1, running: false },
  { type: 'logs', project: 'shop', instance: 'main', records: [record], next: 2 },
  { type: 'gap', project: 'shop', instance: 'main', skipped: 12 },
  { type: 'reset', project: 'shop', instance: 'main' },
  {
    type: 'events',
    events: [
      { project: 'shop', instance: 'main', event: 'spawned', pid: 4242, http_port: 19100, gevent_port: 19101, changes: [] },
      { project: 'shop', instance: 'main', event: 'status', status: { state: 'running', pid: 4242 } },
    ],
  },
  { type: 'snapshot', instances: [running] },
  { type: 'error', error: 'unknown_project', message: 'no project named "x"' },
]

describe('API types', () => {
  it('describe what the server sends', () => {
    expect(project.instances[0]?.live?.status.state).toBe('running')
    expect(record.level).toBe('info')
  })

  it('describe the WebSocket messages', () => {
    expect(WS_PATH).toBe('/api/v1/ws')
    expect(subscribe.op).toBe('subscribe_logs')
    expect(frames.map((f) => f.type)).toContain('logs')
  })
})
