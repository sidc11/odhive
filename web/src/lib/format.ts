// Numbers as the UI shows them.

const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB']

/** `1536` → `1.5 KiB`; one decimal below 10, none above. */
export function formatBytes(bytes: number): string {
  let value = Math.max(bytes, 0)
  let unit = 0
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024
    unit += 1
  }
  if (unit === 0) return `${value} B`
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${UNITS[unit]}`
}

/** CPU use in percent of one core: `0.4` → `0.4%`, `152.3` → `152%`. */
export function formatCpu(percent: number): string {
  const value = Math.max(percent, 0)
  return `${value < 10 ? value.toFixed(1) : Math.round(value)}%`
}

/**
 * Time since `startedAt` (seconds since the epoch) at `nowMs`: the two
 * largest units, e.g. `45s`, `3m 12s`, `2h 05m`, `4d 3h`.
 */
export function formatUptime(startedAt: number, nowMs: number): string {
  const total = Math.max(Math.floor(nowMs / 1000 - startedAt), 0)
  const days = Math.floor(total / 86_400)
  const hours = Math.floor((total % 86_400) / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const seconds = total % 60
  const pad = (n: number): string => String(n).padStart(2, '0')
  if (days > 0) return `${days}d ${hours}h`
  if (hours > 0) return `${hours}h ${pad(minutes)}m`
  if (minutes > 0) return `${minutes}m ${pad(seconds)}s`
  return `${seconds}s`
}

/**
 * A moment as the UI shows it, in the browser's time zone: milliseconds
 * since the epoch or an ISO 8601 string (`2026-11-26T10:04:00Z`). An
 * unreadable value is shown as is. `seconds` for times that may be close
 * together (snapshots).
 */
export function formatTime(
  when: number | string,
  options: { seconds?: boolean; locale?: string; timeZone?: string } = {},
): string {
  const date = new Date(when)
  if (Number.isNaN(date.getTime())) return String(when)
  const { seconds = false, locale, timeZone } = options
  return date.toLocaleString(locale, { dateStyle: 'medium', timeStyle: seconds ? 'medium' : 'short', timeZone })
}
