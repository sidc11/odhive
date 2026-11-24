<script lang="ts">
  import { highlight } from './filter'
  import { formatTime, LEVEL_LABELS, type LogRow } from './rows'
  import { decorate, fileLinks, firstLine, foldable, foldSummary, type FileLink } from './traceback'

  let {
    row,
    lineHeight,
    search = null,
    current = false,
    expanded = false,
    ontoggle,
    oncopy,
    onopen,
  }: {
    row: LogRow
    lineHeight: number
    /** Global search pattern to highlight. */
    search?: RegExp | null
    /** The current search match. */
    current?: boolean
    /** Whether a traceback shows all its lines. */
    expanded?: boolean
    /** Fold or unfold the traceback of row `id`. */
    ontoggle?: (id: number) => void
    /** Copy the traceback of this row; resolves to whether it worked. */
    oncopy?: (row: LogRow) => Promise<boolean>
    /** Open a source location (no links without it). */
    onopen?: (link: FileLink) => void
  } = $props()

  /** How long "Copied" shows (ms). */
  const COPIED_MS = 1500

  const markClass = $derived(current ? 'bg-match-current text-match-current-fg' : 'bg-match text-match-fg')
  const fold = $derived(foldable(row))
  const folded = $derived(fold && !expanded)
  const links = $derived(row.kind === 'entry' && onopen && !folded ? fileLinks(row.record.message) : [])

  let copied: 'no' | 'yes' | 'failed' = $state('no')
  let copiedTimer: ReturnType<typeof setTimeout> | undefined

  async function copy(): Promise<void> {
    if (!oncopy) return
    copied = (await oncopy(row)) ? 'yes' : 'failed'
    clearTimeout(copiedTimer)
    copiedTimer = setTimeout(() => (copied = 'no'), COPIED_MS)
  }

  $effect(() => () => clearTimeout(copiedTimer))

  const COPY_LABELS = { no: 'Copy traceback', yes: 'Copied', failed: 'Copy failed' } as const

  function gapText(skipped: number): string {
    const what = skipped === 1 ? 'entry' : 'entries'
    return `⋯ ${skipped.toLocaleString()} ${what} dropped from memory (still in the log file)`
  }

  /** After the logger name (a literal space at the end of an element is trimmed). */
  const LOGGER_END = ': '
  /** After the fold buttons. */
  const SPACE = ' '

  const LEVEL_CLASSES = {
    debug: 'text-level-debug',
    info: 'text-level-info',
    runbot: 'text-level-info',
    warning: 'text-level-warning',
    error: 'text-level-error',
    critical: 'font-bold text-level-error',
  } as const

  // Buttons stay out of selected text (so copying lines copies the log).
  const TOOL = 'rounded border border-line px-1 text-fg-muted select-none hover:bg-surface-sunken hover:text-fg'
</script>

<!-- One line of markup per row on purpose: whitespace at block edges is trimmed, so separators live inside the spans. -->
{#if row.kind === 'entry'}
  {@const record = row.record}
  <div class={`px-3 whitespace-pre ${current ? 'bg-row-current' : ''}`} style:height={`${(folded ? 1 : row.lines) * lineHeight}px`} data-seq={record.seq} data-current={current || undefined} data-folded={folded || undefined}><span class="text-fg-muted" title={record.timestamp ?? undefined}>{`${formatTime(record.timestamp).padEnd(12)} `}</span>{#if record.level}<span class={LEVEL_CLASSES[record.level]}>{`${LEVEL_LABELS[record.level].padEnd(6)} `}</span>{/if}{#if record.db}<span class="text-fg-muted">{`${record.db} `}</span>{/if}{#if record.logger}<span class="text-fg-muted">{#each highlight(record.logger, search) as part, i (i)}{#if part.match}<mark class={markClass}>{part.text}</mark>{:else}{part.text}{/if}{/each}{LOGGER_END}</span>{/if}{#if fold}<button type="button" class={TOOL} aria-expanded={expanded} aria-label={expanded ? 'Fold traceback' : 'Unfold traceback'} title={expanded ? 'Fold traceback' : 'Unfold traceback'} onclick={() => ontoggle?.(row.id)}>{expanded ? '−' : '+'}</button>{SPACE}<button type="button" class={TOOL} title="Copy the traceback" aria-live="polite" onclick={copy}>{COPY_LABELS[copied]}</button>{SPACE}{/if}{#if folded}{#each highlight(firstLine(record.message), search) as part, i (i)}{#if part.match}<mark class={markClass}>{part.text}</mark>{:else}{part.text}{/if}{/each}<span class="text-fg-muted">{` · ${foldSummary(record, row.lines)}`}</span>{:else}{#each decorate(record.message, search, links) as piece, i (i)}{#if piece.link}{@const link = piece.link}<button type="button" class="cursor-pointer text-accent underline decoration-dotted underline-offset-2 select-text hover:decoration-solid" title={`Open ${link.path}:${link.line} in the editor`} data-path={link.path} data-line={link.line} onclick={() => onopen?.(link)}>{#each piece.parts as part, j (j)}{#if part.match}<mark class={markClass}>{part.text}</mark>{:else}{part.text}{/if}{/each}</button>{:else}{#each piece.parts as part, j (j)}{#if part.match}<mark class={markClass}>{part.text}</mark>{:else}{part.text}{/if}{/each}{/if}{/each}{/if}</div>
{:else if row.kind === 'gap'}
  <div class="px-3 whitespace-pre text-fg-muted italic" style:height={`${lineHeight}px`}>{gapText(row.skipped)}</div>
{:else}
  <div class="px-3 whitespace-pre text-fg-muted" style:height={`${lineHeight}px`}>── Log started over ──</div>
{/if}
