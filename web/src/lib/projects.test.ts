import { describe, expect, it, vi } from 'vitest'
import { project, projectsFetch } from '../test-fixtures'
import { ProjectsStore } from './projects.svelte'

describe('ProjectsStore', () => {
  it('loads the project list from the API', async () => {
    const fetcher = vi.fn(projectsFetch([project('shop')]))
    const store = new ProjectsStore(fetcher)
    expect(store.state.status).toBe('loading')
    await store.load()
    expect(fetcher).toHaveBeenCalledWith('/api/v1/projects', expect.anything())
    expect(store.state).toMatchObject({ status: 'ready', projects: [{ name: 'shop' }] })
    expect(store.find('shop')?.name).toBe('shop')
    expect(store.find('other')).toBeUndefined()
  })

  it('explains a missing sign-in', async () => {
    const store = new ProjectsStore(projectsFetch({ error: 'unauthorized', message: 'sign in' }, 401))
    await store.load()
    expect(store.state).toMatchObject({ status: 'error', unauthorized: true })
    expect(store.state.status === 'error' && store.state.message).toContain('odhive')
  })

  it('shows the API error message, or the status without one', async () => {
    const store = new ProjectsStore(projectsFetch({ error: 'internal', message: 'disk on fire' }, 500))
    await store.load()
    expect(store.state).toMatchObject({ status: 'error', message: 'disk on fire', unauthorized: false })

    const bare = new ProjectsStore(async () => new Response('oops', { status: 502 }))
    await bare.load()
    expect(bare.state).toMatchObject({ status: 'error', message: expect.stringContaining('502') })
  })

  it('reports an unreachable service', async () => {
    const store = new ProjectsStore(async () => {
      throw new TypeError('network')
    })
    await store.load()
    expect(store.state).toMatchObject({ status: 'error', message: expect.stringContaining('not reachable') })
  })
})
