// What an instance card derives from the API shapes.
import type { GitHead, InstanceInfo, PortChange, Status } from '../api'

/** Odoo listens on loopback by default (`http_interface = 127.0.0.1`). */
export function odooUrl(port: number, debugAssets = false): string {
  return `http://127.0.0.1:${port}/web${debugAssets ? '?debug=assets' : ''}`
}

function reasonText(change: PortChange): string {
  switch (change.reason.reason) {
    case 'in_use':
      return `${change.requested} was busy`
    case 'reserved_by':
      return `${change.requested} is used by ${change.reason.owner.project}/${change.reason.owner.instance}`
    case 'other_port':
      return `${change.requested} is this instance's other port`
  }
}

/** The HTTP port moved away from where it would start. */
export function portMove(info: InstanceInfo | null | undefined): { from: number; reason: string } | null {
  const change = info?.port_changes.find((candidate) => candidate.kind === 'http')
  if (!change || info?.http_port !== change.assigned) return null
  return { from: change.requested, reason: `${reasonText(change)}; using ${change.assigned}` }
}

/** Branch of the first repository; others in the title. */
export function gitSummary(heads: GitHead[]): { label: string; title: string } | null {
  const describe = (head: GitHead): string => head.branch ?? `detached at ${head.commit ?? '?'}`
  const first = heads[0]
  if (!first) return null
  const more = heads.length > 1 ? ` +${heads.length - 1}` : ''
  return {
    label: `${describe(first)}${more}`,
    title: heads.map((head) => `${head.repo}: ${describe(head)}`).join('\n'),
  }
}

/** Why the last run ended badly, if it did. */
export function exitSummary(status: Status | undefined): string | null {
  if (!status) return null
  if (status.state === 'failed_to_start') return `Failed to start: ${status.error}`
  if (status.state !== 'crashed') return null
  const { exit } = status
  if (exit.port_in_use) return 'Crashed: a port was already in use'
  if (exit.signal !== null && exit.signal !== undefined) return `Crashed: killed by signal ${exit.signal}`
  if (exit.code !== null && exit.code !== undefined) return `Crashed with exit code ${exit.code}`
  return 'Crashed'
}
