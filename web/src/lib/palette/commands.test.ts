import { describe, expect, it, vi } from 'vitest'
import type { ProjectsState } from '../projects.svelte'
import { instanceView, live, project } from '../../test-fixtures'
import { buildCommands, fuzzyScore, highlight, MAX_RESULTS, search, type Command } from './commands'

const READY: ProjectsState = {
  status: 'ready',
  projects: [
    project('shop', [
      instanceView('main', { live: live('shop', 'main', { state: 'running', pid: 1 }) }),
      instanceView('test'),
    ]),
    project('crm', [instanceView('main')], { has_venv: false }),
  ],
}

function deps() {
  return { navigate: vi.fn(), act: vi.fn(), settings: vi.fn() }
}

const ids = (commands: { id: string }[]): string[] => commands.map((command) => command.id)

describe('buildCommands', () => {
  it('always offers the pages and the wizard', () => {
    const commands = buildCommands({ status: 'loading' }, deps())
    expect(ids(commands)).toEqual(['page-dashboard', 'page-environment', 'page-logs', 'new-project'])
  })

  it('lists projects, instances and the actions each instance allows', () => {
    const commands = buildCommands(READY, deps())
    expect(ids(commands)).toEqual([
      'page-dashboard',
      'page-environment',
      'page-logs',
      'project:shop',
      'project:crm',
      'instance:shop/main',
      'instance:shop/test',
      'instance:crm/main',
      'new-project',
      // Odoo options per project, then per instance. Running: stop and
      // restart, no start. Stopped: start only. No venv: nothing to run.
      // Settings: always.
      'config:shop',
      'stop:shop/main',
      'restart:shop/main',
      'logs:shop/main',
      'settings:shop/main',
      'start:shop/test',
      'logs:shop/test',
      'settings:shop/test',
      'config:crm',
      'logs:crm/main',
      'settings:crm/main',
    ])
    const main = commands.find((command) => command.id === 'instance:shop/main')
    expect(main).toMatchObject({ group: 'Instances', title: 'shop/main', state: 'running', hint: 'Database db_main' })
  })

  it('opens the Odoo options of a project', () => {
    const d = deps()
    const config = buildCommands(READY, d).find((command) => command.id === 'config:shop')
    expect(config?.title).toBe('Odoo options of shop')
    config?.run()
    expect(d.navigate).toHaveBeenCalledWith('/projects/shop/config')
  })

  it('keeps each group together, whatever the number of projects', () => {
    const groups = buildCommands(READY, deps()).map((command) => command.group)
    expect(groups.filter((group, i) => group !== groups[i - 1])).toEqual(['Pages', 'Projects', 'Instances', 'Actions'])
  })

  it('a project that failed to load cannot be started', () => {
    const broken: ProjectsState = {
      status: 'ready',
      projects: [project('old', [instanceView('main')], { error: 'bad odhive.toml' })],
    }
    expect(ids(buildCommands(broken, deps())).filter((id) => id.startsWith('start:'))).toEqual([])
  })

  it('runs navigation and actions through its dependencies', () => {
    const d = deps()
    const commands = buildCommands(READY, d)
    const byId = (id: string): Command => commands.find((command) => command.id === id) as Command
    byId('new-project').run()
    byId('project:shop').run()
    byId('instance:shop/test').run()
    byId('start:shop/test').run()
    byId('restart:shop/main').run()
    byId('settings:crm/main').run()
    byId('page-logs').run()
    byId('logs:shop/test').run()
    expect(d.navigate.mock.calls).toEqual([
      ['/new'],
      ['/projects/shop'],
      ['/projects/shop/instances/test'],
      ['/logs'],
      ['/logs/shop/test'],
    ])
    expect(d.act.mock.calls).toEqual([
      ['start', 'shop', 'test'],
      ['restart', 'shop', 'main'],
    ])
    expect(d.settings.mock.calls).toEqual([['crm', 'main']])
    expect(search(commands, 'settings crm')[0]?.command.id).toBe('settings:crm/main')
  })

  it('encodes names in links', () => {
    const d = deps()
    const odd: ProjectsState = { status: 'ready', projects: [project('a b', [instanceView('x%y')])] }
    buildCommands(odd, d)
      .find((command) => command.id === 'instance:a b/x%y')
      ?.run()
    expect(d.navigate).toHaveBeenCalledWith('/projects/a%20b/instances/x%25y')
  })
})

describe('fuzzyScore', () => {
  it('matches a subsequence, case-insensitively', () => {
    expect(fuzzyScore('shmn', 'shop/main')?.positions).toEqual([0, 1, 5, 8])
    expect(fuzzyScore('SHOP'.toLowerCase(), 'Shop')?.positions).toEqual([0, 1, 2, 3])
    expect(fuzzyScore('xyz', 'shop/main')).toBeNull()
    expect(fuzzyScore('niam', 'main')).toBeNull()
  })

  it('prefers word starts over letters inside words', () => {
    // "m" of "main", not the "m" inside "demo".
    expect(fuzzyScore('dm', 'demo/main')?.positions).toEqual([0, 5])
    // Both runs start words; the tighter one wins.
    expect(fuzzyScore('sm', 'start shop/main')?.positions).toEqual([6, 11])
    expect(fuzzyScore('sm', 'stash/main')?.positions).toEqual([0, 6])
  })

  it('does not skip ahead to a word start it cannot finish from', () => {
    // Taking the "s" of "shop" would leave no "t" after it.
    expect(fuzzyScore('rstshmain', 'restart shop/main')?.positions).toEqual([0, 2, 3, 8, 9, 13, 14, 15, 16])
  })

  it('finds the best start, not only the first', () => {
    // The first "m" is inside "demo"; starting at "main" scores better.
    expect(fuzzyScore('main', 'demo/main')?.positions).toEqual([5, 6, 7, 8])
  })

  it('scores consecutive and word-start matches higher', () => {
    const tight = fuzzyScore('shop', 'shop/main')?.score ?? 0
    const loose = fuzzyScore('shop', 'stash hoop')?.score ?? 0
    expect(tight).toBeGreaterThan(loose)
  })
})

describe('search', () => {
  const commands = buildCommands(READY, deps())

  it('returns everything in list order for an empty query', () => {
    expect(ids(search(commands, '  ').map((match) => match.command))).toEqual(ids(commands))
  })

  it('ranks the closest title first and ignores spaces in the query', () => {
    const results = search(commands, 'restart shop main').map((match) => match.command.id)
    expect(results[0]).toBe('restart:shop/main')
    expect(search(commands, 'stop')[0]?.command.id).toBe('stop:shop/main')
    expect(search(commands, 'crm')[0]?.command.id).toBe('project:crm')
    expect(search(commands, 'crm main')[0]?.command.id).toBe('instance:crm/main')
  })

  it('finds commands by keyword and hint, below title matches', () => {
    expect(search(commands, 'postgres').map((match) => match.command.id)).toEqual(['page-environment'])
    const create = search(commands, 'wizard')
    expect(create[0]?.command.id).toBe('new-project')
    expect(create[0]?.positions).toEqual([])
    // "db_test" is the keyword of shop/test.
    expect(search(commands, 'db_test')[0]?.command.id).toBe('instance:shop/test')
  })

  it('caps the number of results', () => {
    const many: Command[] = Array.from({ length: MAX_RESULTS + 10 }, (_, i) => ({
      id: `c${i}`,
      group: 'Pages',
      title: `Command ${i}`,
      run: () => {},
    }))
    expect(search(many, 'command')).toHaveLength(MAX_RESULTS)
  })
})

describe('highlight', () => {
  it('splits the title into marked runs', () => {
    expect(highlight('shop/main', [0, 1, 5])).toEqual([
      { text: 'sh', mark: true },
      { text: 'op/', mark: false },
      { text: 'm', mark: true },
      { text: 'ain', mark: false },
    ])
    expect(highlight('abc', [])).toEqual([{ text: 'abc', mark: false }])
  })
})
