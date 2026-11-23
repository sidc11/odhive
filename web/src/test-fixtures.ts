// Shapes the API sends, for component and store tests.
import type { InstanceInfo, InstanceView, Project, Status } from './lib/api'

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
    status,
  }
}

export function project(name: string, instances: InstanceView[] = [], extra: Partial<Project> = {}): Project {
  return {
    name,
    path: `/home/dev/OdHive/projects/${name}`,
    addons_paths: [],
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
