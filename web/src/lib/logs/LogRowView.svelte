<script lang="ts">
  import { highlight } from './filter'
  import { formatTime, LEVEL_LABELS, type LogRow } from './rows'

  let {
    row,
    lineHeight,
    search = null,
    current = false,
  }: {
    row: LogRow
    lineHeight: number
    /** Global search pattern to highlight. */
    search?: RegExp | null
    /** The current search match. */
    current?: boolean
  } = $props()

  const markClass = $derived(current ? 'bg-match-current text-match-current-fg' : 'bg-match text-match-fg')

  function gapText(skipped: number): string {
    const what = skipped === 1 ? 'entry' : 'entries'
    return `⋯ ${skipped.toLocaleString()} ${what} dropped from memory (still in the log file)`
  }

  /** After the logger name (a literal space at the end of an element is trimmed). */
  const LOGGER_END = ': '

  const LEVEL_CLASSES = {
    debug: 'text-level-debug',
    info: 'text-level-info',
    runbot: 'text-level-info',
    warning: 'text-level-warning',
    error: 'text-level-error',
    critical: 'font-bold text-level-error',
  } as const
</script>

{#if row.kind === 'entry'}
  {@const record = row.record}
  <!-- Each field carries its trailing space: whitespace at block edges is trimmed. -->
  <div class={`px-3 whitespace-pre ${current ? 'bg-row-current' : ''}`} style:height={`${row.lines * lineHeight}px`} data-seq={record.seq} data-current={current || undefined}><span class="text-fg-muted" title={record.timestamp ?? undefined}>{`${formatTime(record.timestamp).padEnd(12)} `}</span>{#if record.level}<span class={LEVEL_CLASSES[record.level]}>{`${LEVEL_LABELS[record.level].padEnd(6)} `}</span>{/if}{#if record.db}<span class="text-fg-muted">{`${record.db} `}</span>{/if}{#if record.logger}<span class="text-fg-muted">{#each highlight(record.logger, search) as part, i (i)}{#if part.match}<mark class={markClass}>{part.text}</mark>{:else}{part.text}{/if}{/each}{LOGGER_END}</span>{/if}{#each highlight(record.message, search) as part, i (i)}{#if part.match}<mark class={markClass}>{part.text}</mark>{:else}{part.text}{/if}{/each}</div>
{:else if row.kind === 'gap'}
  <div class="px-3 whitespace-pre text-fg-muted italic" style:height={`${lineHeight}px`}>{gapText(row.skipped)}</div>
{:else}
  <div class="px-3 whitespace-pre text-fg-muted" style:height={`${lineHeight}px`}>── Log started over ──</div>
{/if}
