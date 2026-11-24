// What the command palette offers, and the fuzzy search over it. Pure
// functions: the component only renders and runs the result.
import type { InstanceAction } from '../actions.svelte'
import type { ProjectsState } from '../projects.svelte'
import { routeHref } from '../router.svelte'
import { allowedActions, instanceState, type InstanceState } from '../status'

export type CommandGroup = 'Pages' | 'Projects' | 'Instances' | 'Actions'

export interface Command {
  /** Stable and unique; also the option's DOM id suffix. */
  id: string
  group: CommandGroup
  title: string
  /** Secondary text, e.g. what an entry does. */
  hint?: string
  /** Shown as a status dot. */
  state?: InstanceState
  /** More words that find the command (not highlighted). */
  keywords?: string[]
  run: () => void
}

export interface CommandDeps {
  navigate: (href: string) => void
  act: (action: InstanceAction, project: string, instance: string) => void
}

const ACTION_TITLE: Record<InstanceAction, string> = { start: 'Start', stop: 'Stop', restart: 'Restart' }
const ACTION_ORDER: InstanceAction[] = ['start', 'stop', 'restart']

export function buildCommands(state: ProjectsState, deps: CommandDeps): Command[] {
  const { navigate, act } = deps
  const commands: Command[] = [
    {
      id: 'page-dashboard',
      group: 'Pages',
      title: 'Dashboard',
      keywords: ['home', 'instances'],
      run: () => navigate(routeHref({ name: 'dashboard' })),
    },
    {
      id: 'page-environment',
      group: 'Pages',
      title: 'Environment',
      hint: 'PostgreSQL, Python, wkhtmltopdf, git, uv',
      keywords: ['check', 'postgresql', 'python', 'wkhtmltopdf', 'git', 'uv'],
      run: () => navigate(routeHref({ name: 'environment' })),
    },
  ]
  // Groups stay together: pages, projects, instances, then actions.
  const actionCommands: Command[] = [
    {
      id: 'new-project',
      group: 'Actions',
      title: 'New project',
      hint: 'Open the project wizard',
      keywords: ['create', 'add', 'wizard'],
      run: () => navigate(routeHref({ name: 'new_project' })),
    },
  ]
  if (state.status !== 'ready') return [...commands, ...actionCommands]

  const instanceCommands: Command[] = []
  for (const project of state.projects) {
    commands.push({
      id: `project:${project.name}`,
      group: 'Projects',
      title: project.name,
      hint: project.odoo_version ? `Odoo ${project.odoo_version}` : 'Project',
      run: () => navigate(routeHref({ name: 'project', project: project.name })),
    })
    for (const view of project.instances) {
      const name = `${project.name}/${view.id}`
      const status = instanceState(view)
      instanceCommands.push({
        id: `instance:${name}`,
        group: 'Instances',
        title: name,
        hint: view.db ? `Database ${view.db}` : undefined,
        state: status,
        keywords: view.db ? [view.db] : undefined,
        run: () => navigate(routeHref({ name: 'instance', project: project.name, instance: view.id })),
      })
      const allowed = allowedActions(project, view)
      for (const action of ACTION_ORDER) {
        if (!allowed[action]) continue
        actionCommands.push({
          id: `${action}:${name}`,
          group: 'Actions',
          title: `${ACTION_TITLE[action]} ${name}`,
          state: status,
          run: () => act(action, project.name, view.id),
        })
      }
    }
  }
  return [...commands, ...instanceCommands, ...actionCommands]
}

export interface Match {
  command: Command
  /** Indices in `command.title` to highlight. */
  positions: number[]
  score: number
}

/** Most results shown at once. */
export const MAX_RESULTS = 50

function isBoundary(text: string, index: number): boolean {
  if (index === 0) return true
  return /[\s/_.\-:]/.test(text[index - 1] ?? '')
}

/** Longest title or keyword scored; longer text is cut (titles are short). */
const MAX_TEXT = 200

/**
 * Scores `needle` (lower case, no spaces) as a subsequence of `haystack`,
 * choosing the best alignment: each matched character counts, word starts
 * and runs of consecutive characters more; gaps, a late start and extra
 * length less.
 */
export function fuzzyScore(needle: string, haystack: string): { score: number; positions: number[] } | null {
  if (needle === '') return { score: 0, positions: [] }
  const text = haystack.slice(0, MAX_TEXT).toLowerCase()
  const n = needle.length
  const m = text.length
  if (n > m) return null
  // best[i][j]: best score with needle[i] matched at text[j]; from[i][j]: where needle[i-1] was.
  const best: number[][] = []
  const from: number[][] = []
  for (let i = 0; i < n; i += 1) {
    const row = new Array<number>(m).fill(-Infinity)
    const back = new Array<number>(m).fill(-1)
    for (let j = i; j < m; j += 1) {
      if (text[j] !== needle[i]) continue
      const own = 1 + (isBoundary(text, j) ? 4 : 0)
      if (i === 0) {
        row[j] = own - j * 0.05
        continue
      }
      const previous = best[i - 1] ?? []
      for (let k = i - 1; k < j; k += 1) {
        const base = previous[k] ?? -Infinity
        if (base === -Infinity) continue
        const link = k === j - 1 ? 4 : -Math.min(j - k - 1, 10) * 0.1
        if (base + own + link > (row[j] ?? -Infinity)) {
          row[j] = base + own + link
          back[j] = k
        }
      }
    }
    best.push(row)
    from.push(back)
  }
  const last = best[n - 1] ?? []
  let end = -1
  for (let j = 0; j < m; j += 1) if ((last[j] ?? -Infinity) > (last[end] ?? -Infinity)) end = j
  if (end === -1) return null
  const positions: number[] = []
  for (let i = n - 1, j = end; i >= 0; i -= 1) {
    positions.push(j)
    j = from[i]?.[j] ?? -1
  }
  // Of equal matches, the shorter text is the closer one.
  return { score: (last[end] ?? 0) - (m - n) * 0.02, positions: positions.reverse() }
}

/** Commands matching `query`, best first (ties keep the list order). */
export function search(commands: Command[], query: string): Match[] {
  const needle = query.toLowerCase().replace(/\s+/g, '')
  const matches: (Match & { order: number })[] = []
  commands.forEach((command, order) => {
    const inTitle = fuzzyScore(needle, command.title)
    if (inTitle) {
      matches.push({ command, positions: inTitle.positions, score: inTitle.score, order })
      return
    }
    // Keywords and the hint find a command too, ranked below title hits.
    let other: number | null = null
    for (const text of [...(command.keywords ?? []), command.hint ?? '']) {
      const found = fuzzyScore(needle, text)
      if (found && (other === null || found.score > other)) other = found.score
    }
    if (other !== null) matches.push({ command, positions: [], score: other / 2, order })
  })
  if (needle !== '') matches.sort((a, b) => b.score - a.score || a.order - b.order)
  return matches.slice(0, MAX_RESULTS).map(({ command, positions, score }) => ({ command, positions, score }))
}

/** `title` cut into plain and highlighted parts (rendered as text, never HTML). */
export function highlight(title: string, positions: number[]): { text: string; mark: boolean }[] {
  const marked = new Set(positions)
  const parts: { text: string; mark: boolean }[] = []
  for (let i = 0; i < title.length; i += 1) {
    const mark = marked.has(i)
    const last = parts[parts.length - 1]
    if (last && last.mark === mark) last.text += title[i]
    else parts.push({ text: title[i] ?? '', mark })
  }
  return parts
}
