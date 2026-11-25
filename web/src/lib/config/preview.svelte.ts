// Live `odoo.conf` preview: the form's options rendered by the server (as a
// start would write them, secrets masked) next to the saved file. Requests
// are debounced and only the newest answer is kept.
import type { OptionsUpdate } from '../api'
import { ApiRequestError, type ApiClient } from '../api/client'
import { changeCount, diffLines, type DiffLine } from './diff'

type PreviewApi = Pick<ApiClient, 'previewProjectOptions'>

/** Wait after the last change before asking for a preview. */
export const PREVIEW_DELAY_MS = 250

export class ConfPreview {
  text = $state('')
  saved = $state('')
  error = $state<string | null>(null)
  /** A request is waiting or running. */
  pending = $state(false)
  readonly diff: DiffLine[] = $derived(diffLines(this.saved, this.text))
  readonly changes: number = $derived(changeCount(this.diff))

  private readonly api: PreviewApi
  private readonly project: string
  private readonly delay: number
  private timer: ReturnType<typeof setTimeout> | undefined
  private seq = 0

  constructor(api: PreviewApi, project: string, delay = PREVIEW_DELAY_MS) {
    this.api = api
    this.project = project
    this.delay = delay
  }

  /** Asks for a preview of `update` once changes settle. */
  request(update: OptionsUpdate): void {
    clearTimeout(this.timer)
    const seq = ++this.seq
    this.pending = true
    this.timer = setTimeout(() => void this.fetch(update, seq), this.delay)
  }

  /** Drops any waiting request (e.g. while the form has invalid values). */
  hold(reason: string): void {
    clearTimeout(this.timer)
    this.seq++
    this.pending = false
    this.error = reason
  }

  dispose(): void {
    clearTimeout(this.timer)
    this.seq++
  }

  private async fetch(update: OptionsUpdate, seq: number): Promise<void> {
    try {
      const answer = await this.api.previewProjectOptions(this.project, update)
      if (seq !== this.seq) return
      this.text = answer.text
      this.saved = answer.saved
      this.error = null
    } catch (error) {
      if (seq !== this.seq) return
      this.error = error instanceof ApiRequestError ? error.message : String(error)
    } finally {
      if (seq === this.seq) this.pending = false
    }
  }
}
