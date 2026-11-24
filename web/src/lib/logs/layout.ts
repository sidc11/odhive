// Vertical layout of a virtualized list whose rows are a whole number of
// text lines high. Every position is in lines; the view multiplies by the
// measured line height, so no row ever has to be measured.

export interface LineRange {
  /** First row to render. */
  start: number
  /** One past the last row to render. */
  end: number
}

export class RowLayout {
  /** `offsets[i]` = lines above row `i`; `offsets[length]` = total. */
  private offsets: number[] = [0]

  private at(index: number): number {
    return this.offsets[index] ?? 0
  }

  get length(): number {
    return this.offsets.length - 1
  }

  /** Lines of all rows. */
  get total(): number {
    return this.at(this.offsets.length - 1)
  }

  /** Lines above row `index`. */
  offsetOf(index: number): number {
    return this.at(Math.max(0, Math.min(index, this.length)))
  }

  /** Lines of row `index`. */
  linesOf(index: number): number {
    return this.at(index + 1) - this.at(index)
  }

  /** Adds rows of these heights at the end. */
  append(lines: Iterable<number>): void {
    let total = this.total
    for (const height of lines) {
      total += Math.max(1, height)
      this.offsets.push(total)
    }
  }

  /** Sets row `index` to `lines` lines high; returns the change in lines. */
  resize(index: number, lines: number): number {
    if (index < 0 || index >= this.length) return 0
    const delta = Math.max(1, lines) - this.linesOf(index)
    if (delta !== 0) for (let i = index + 1; i < this.offsets.length; i += 1) this.offsets[i] = this.at(i) + delta
    return delta
  }

  /** Removes the first `count` rows; returns the lines they took. */
  dropFront(count: number): number {
    const n = Math.min(Math.max(count, 0), this.length)
    if (n === 0) return 0
    const removed = this.at(n)
    this.offsets = this.offsets.slice(n).map((offset) => offset - removed)
    return removed
  }

  /** Replaces every row. */
  reset(lines: Iterable<number> = []): void {
    this.offsets = [0]
    this.append(lines)
  }

  /** Row covering line `line` (clamped to the rows that exist). */
  indexAt(line: number): number {
    const last = this.length - 1
    if (last < 0) return 0
    if (line <= 0) return 0
    if (line >= this.total) return last
    // Largest i with offsets[i] <= line.
    let low = 0
    let high = last
    while (low < high) {
      const mid = (low + high + 1) >> 1
      if (this.at(mid) <= line) low = mid
      else high = mid - 1
    }
    return low
  }

  /**
   * Rows to render for a viewport `height` lines high scrolled to line
   * `top`, with `overscan` extra lines above and below.
   */
  range(top: number, height: number, overscan = 0): LineRange {
    if (this.length === 0) return { start: 0, end: 0 }
    const start = this.indexAt(top - overscan)
    const bottom = top + height + overscan
    let end = this.indexAt(bottom) + 1
    if (end > this.length) end = this.length
    return { start, end }
  }
}
