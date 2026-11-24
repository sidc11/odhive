// The split log view: which logs it shows, and how they are laid out.
import { MAX_PANES, routeHref, type LogPane } from '../router.svelte'

export interface KeyedPane extends LogPane {
  /**
   * Stable while the pane stays: the instance plus how many panes of it
   * come before (the same instance may be shown twice, filtered apart).
   */
  key: string
}

export function keyedPanes(panes: LogPane[]): KeyedPane[] {
  const seen = new Map<string, number>()
  return panes.map((pane) => {
    const name = `${pane.project}/${pane.instance}`
    const count = seen.get(name) ?? 0
    seen.set(name, count + 1)
    return { ...pane, key: `${name}#${count}` }
  })
}

/** The split view with `pane` added at the end (unchanged when full). */
export function withPane(panes: LogPane[], pane: LogPane): string {
  const next = panes.length >= MAX_PANES ? panes : [...panes, pane]
  return routeHref({ name: 'logs', panes: next })
}

/** The split view without the pane at `index`. */
export function withoutPane(panes: LogPane[], index: number): string {
  return routeHref({ name: 'logs', panes: panes.filter((_, i) => i !== index) })
}

/**
 * Grid classes by pane count. Below `lg` panes stack (each gets its own
 * height and the page scrolls); 2 and 4 go two wide, 3 go three wide only
 * where each still has room for the toolbar (~360px), else 2 + 1 wide.
 * Rows share the page height but keep 20rem each (on a short screen the
 * page scrolls rather than leave a few log lines per pane).
 * Whole class strings, so Tailwind finds them.
 */
const ONE_ROW = 'lg:grid-rows-[minmax(20rem,1fr)]'
const TWO_ROWS = 'lg:grid-rows-[repeat(2,minmax(20rem,1fr))]'
const GRIDS: Record<number, string> = {
  1: ONE_ROW,
  2: `lg:grid-cols-2 ${ONE_ROW}`,
  3: `lg:grid-cols-2 ${TWO_ROWS} 2xl:grid-cols-3 2xl:grid-rows-[minmax(20rem,1fr)]`,
  4: `lg:grid-cols-2 ${TWO_ROWS}`,
}

export function gridClass(count: number): string {
  return GRIDS[Math.min(Math.max(count, 1), MAX_PANES)] ?? ''
}

/** Extra classes of the pane at `index` (the third of three spans the row until it fits). */
export function paneClass(count: number, index: number): string {
  return count === 3 && index === 2 ? 'lg:col-span-2 2xl:col-span-1' : ''
}
