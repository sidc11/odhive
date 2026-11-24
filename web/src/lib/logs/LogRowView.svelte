<script lang="ts">
  import { formatTime, LEVEL_LABELS, type LogRow } from './rows'

  let { row, lineHeight }: { row: LogRow; lineHeight: number } = $props()

  function gapText(skipped: number): string {
    const what = skipped === 1 ? 'entry' : 'entries'
    return `⋯ ${skipped.toLocaleString()} ${what} dropped from memory (still in the log file)`
  }

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
  <div class="px-3 whitespace-pre" style:height={`${row.lines * lineHeight}px`} data-seq={record.seq}><span class="text-fg-muted" title={record.timestamp ?? undefined}>{`${formatTime(record.timestamp).padEnd(12)} `}</span>{#if record.level}<span class={LEVEL_CLASSES[record.level]}>{`${LEVEL_LABELS[record.level].padEnd(6)} `}</span>{/if}{#if record.db}<span class="text-fg-muted">{`${record.db} `}</span>{/if}{#if record.logger}<span class="text-fg-muted">{`${record.logger}: `}</span>{/if}{record.message}</div>
{:else if row.kind === 'gap'}
  <div class="px-3 whitespace-pre text-fg-muted italic" style:height={`${lineHeight}px`}>{gapText(row.skipped)}</div>
{:else}
  <div class="px-3 whitespace-pre text-fg-muted" style:height={`${lineHeight}px`}>── Log started over ──</div>
{/if}
