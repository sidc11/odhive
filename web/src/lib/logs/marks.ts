// Lines OdHive wrote into an instance's log (start, Smart Reload restarts,
// dependency installs), shown as events inline with the instance's output.
// Only records the server marked count: text that merely looks like one
// (an Odoo module printing "odhive: …") stays an ordinary line.
import type { LogRecord } from '../api'

export type Marker = NonNullable<LogRecord['mark']>
export type MarkKind = Marker['kind']

export type EventTone = 'muted' | 'accent' | 'ok' | 'warning' | 'error'

export interface EventView {
  /** Short label in the level column. */
  tag: string
  /** Decorative symbol in front of the text (`''` for none). */
  icon: string
  text: string
  tone: EventTone
}

/** Changed files named in a reload event; the rest are counted. */
export const MAX_SHOWN_FILES = 3

function changed(files: readonly string[], count: number): string {
  // No count: the watcher lost events, anything may have changed.
  if (count <= 0) return 'files changed'
  if (files.length === 0) return count === 1 ? '1 file changed' : `${count} files changed`
  if (count === 1) return `${files[0]} changed`
  const shown = files.slice(0, MAX_SHOWN_FILES)
  const more = count - shown.length
  return `${count} files changed (${shown.join(', ')}${more > 0 ? `, +${more} more` : ''})`
}

/** How a marked record is shown. */
export function eventView(marker: Marker, message: string): EventView {
  switch (marker.kind) {
    case 'started':
      return { tag: 'START', icon: '▶', text: `started (pid ${marker.pid})`, tone: 'muted' }
    case 'reload': {
      const how = marker.update.length > 0 ? `restarted with -u ${marker.update.join(',')}` : 'restarted'
      return { tag: 'RELOAD', icon: '↻', text: `${how}: ${changed(marker.files, marker.file_count)}`, tone: 'accent' }
    }
    case 'installing':
      return {
        tag: 'FIX',
        icon: '📦',
        text: `installing ${marker.package} for missing module ${marker.module} (${marker.attempt}/${marker.max})`,
        tone: 'accent',
      }
    case 'install_output':
      return { tag: 'FIX', icon: '', text: message, tone: 'muted' }
    case 'installed':
      return { tag: 'FIX', icon: '📦', text: `installed ${marker.package}, restarting`, tone: 'ok' }
    case 'install_failed':
      return { tag: 'FIX', icon: '✖', text: `install of ${marker.package} failed`, tone: 'error' }
    case 'install_cancelled':
      return { tag: 'FIX', icon: '⊘', text: `install of ${marker.package} cancelled`, tone: 'warning' }
  }
}

/** The event a record shows, `null` for the instance's own output. */
export function eventOf(record: Pick<LogRecord, 'mark' | 'message'>): EventView | null {
  return record.mark ? eventView(record.mark, record.message) : null
}

/**
 * Events shown whatever the level and logger filters: the context for the
 * lines around them. The installer's output lines follow the INFO chip.
 */
export function alwaysShown(record: Pick<LogRecord, 'mark'>): boolean {
  return !!record.mark && record.mark.kind !== 'install_output'
}

/** The text a record shows (and search looks at) after its header. */
export function shownMessage(record: Pick<LogRecord, 'mark' | 'message'>): string {
  return eventOf(record)?.text ?? record.message
}

export const TONE_CLASSES: Record<EventTone, string> = {
  muted: 'text-fg-muted',
  accent: 'text-accent',
  ok: 'text-state-running',
  warning: 'text-level-warning',
  error: 'text-level-error',
}
