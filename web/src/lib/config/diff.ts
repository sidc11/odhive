// Line diff of two small texts (an `odoo.conf` has ~100 lines), by longest
// common subsequence. Removed lines come before added ones at each change.

export interface DiffLine {
  kind: 'same' | 'add' | 'del'
  text: string
}

/** Largest input (lines × lines) diffed; beyond it every line counts as changed. */
export const MAX_CELLS = 4_000_000

function lines(text: string): string[] {
  if (text === '') return []
  const out = text.split('\n')
  if (out[out.length - 1] === '') out.pop()
  return out
}

export function diffLines(before: string, after: string): DiffLine[] {
  const a = lines(before)
  const b = lines(after)
  if (a.length * b.length > MAX_CELLS) {
    return [...a.map((text) => ({ kind: 'del' as const, text })), ...b.map((text) => ({ kind: 'add' as const, text }))]
  }
  // lcs[i][j] = common lines of a[i..] and b[j..].
  const width = b.length + 1
  const lcs = new Uint32Array((a.length + 1) * width)
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      lcs[i * width + j] =
        a[i] === b[j]
          ? (lcs[(i + 1) * width + j + 1] ?? 0) + 1
          : Math.max(lcs[(i + 1) * width + j] ?? 0, lcs[i * width + j + 1] ?? 0)
    }
  }
  const out: DiffLine[] = []
  let i = 0
  let j = 0
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      out.push({ kind: 'same', text: a[i] as string })
      i++
      j++
    } else if ((lcs[(i + 1) * width + j] ?? 0) >= (lcs[i * width + j + 1] ?? 0)) {
      out.push({ kind: 'del', text: a[i++] as string })
    } else {
      out.push({ kind: 'add', text: b[j++] as string })
    }
  }
  while (i < a.length) out.push({ kind: 'del', text: a[i++] as string })
  while (j < b.length) out.push({ kind: 'add', text: b[j++] as string })
  return out
}

/** Number of added and removed lines. */
export function changeCount(diff: readonly DiffLine[]): number {
  return diff.filter((line) => line.kind !== 'same').length
}
