// Projects for the navigation. Loaded once for now; 6.2 replaces the fetch
// with the API client and keeps the list live over the WebSocket.
import { API_PREFIX, type ApiError, type Project } from './api'

export type ProjectsState =
  | { status: 'loading' }
  | { status: 'ready'; projects: Project[] }
  | { status: 'error'; message: string; unauthorized: boolean }

function errorMessage(body: unknown, fallback: string): string {
  if (typeof body === 'object' && body !== null && 'message' in body) {
    const message = (body as ApiError).message
    if (typeof message === 'string' && message !== '') return message
  }
  return fallback
}

export class ProjectsStore {
  state = $state<ProjectsState>({ status: 'loading' })
  private readonly fetcher: typeof fetch

  constructor(fetcher: typeof fetch = (...args) => fetch(...args)) {
    this.fetcher = fetcher
  }

  async load(): Promise<void> {
    let response: Response
    try {
      response = await this.fetcher(`${API_PREFIX}/projects`, {
        headers: { Accept: 'application/json' },
      })
    } catch {
      this.state = {
        status: 'error',
        message: 'OdHive is not reachable. Is the service running?',
        unauthorized: false,
      }
      return
    }
    const body: unknown = await response.json().catch(() => null)
    if (response.ok && Array.isArray(body)) {
      this.state = { status: 'ready', projects: body as Project[] }
      return
    }
    const unauthorized = response.status === 401
    this.state = {
      status: 'error',
      message: unauthorized
        ? 'Not signed in. Run `odhive` in a terminal to open a signed-in tab.'
        : errorMessage(body, `Loading projects failed (HTTP ${response.status}).`),
      unauthorized,
    }
  }

  find(name: string): Project | undefined {
    return this.state.status === 'ready'
      ? this.state.projects.find((project) => project.name === name)
      : undefined
  }
}

export const projects = new ProjectsStore()
