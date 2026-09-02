<script lang="ts">
  /**
   * SyncStatusRow — one settings-panel row showing a sync status
   * with a "Sync now" button. Used twice in NextcloudSettings:
   * once for Contacts (CardDAV) and once for Calendars (CalDAV),
   * so both surfaces stay visually identical and any future
   * sync-able data type plugs in by passing the same props.
   *
   * The component is presentation-only: it doesn't talk to the
   * backend itself. The parent owns the sync invocation and
   * passes the resulting state in.
   */

  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  interface Props {
    /** Row label — "Contacts" / "Calendars" / future categories. */
    label: string
    /** Total cached items for context — "12 cached". When zero
        the chip is dropped so a brand-new account doesn't show
        "0 cached" before its first sync. */
    count?: number | null
    /** RFC 3339 timestamp of the last successful sync, or `null`
        if the account has never finished one. */
    lastSyncedAt?: string | null
    /** True while a sync is in flight — disables the button and
        swaps its icon to `loading`. */
    syncing?: boolean
    /** Most recent sync error to surface, or empty/null for none. */
    error?: string | null
    /** Click handler for the "Sync now" button. Required — without
        it the row is read-only and the button gets hidden. */
    onsync?: () => void
  }

  let {
    label,
    count = null,
    lastSyncedAt = null,
    syncing = false,
    error = null,
    onsync,
  }: Props = $props()

  /** Format a sync timestamp as a relative phrase ("just now",
      "12m ago", "3d ago"). Computed inside an `$effect` so it
      ticks once a minute and the row updates without the parent
      having to push a refresh signal — feels alive without being
      a perf concern (one closure, one setInterval per mount). */
  let now = $state(Date.now())
  $effect(() => {
    const t = window.setInterval(() => {
      now = Date.now()
    }, 60_000)
    return () => window.clearInterval(t)
  })

  const relative = $derived.by(() => {
    if (!lastSyncedAt) return 'Never synced'
    const ts = Date.parse(lastSyncedAt)
    if (Number.isNaN(ts)) return 'Never synced'
    const diffMs = now - ts
    if (diffMs < 60_000) return 'Synced just now'
    const mins = Math.floor(diffMs / 60_000)
    if (mins < 60) return `Synced ${mins}m ago`
    const hours = Math.floor(mins / 60)
    if (hours < 24) return `Synced ${hours}h ago`
    return `Synced ${Math.floor(hours / 24)}d ago`
  })
</script>

<div
  class="flex items-center justify-between pt-2 border-t border-surface-300/40 dark:border-surface-700/60"
>
  <div class="text-sm min-w-0">
    <p class="font-medium truncate">
      {label}
      {#if count !== null && count > 0}
        <span class="text-surface-500 font-normal">· {count} cached</span>
      {/if}
    </p>
    <p class="text-xs text-surface-500">
      {syncing ? 'Syncing…' : relative}
    </p>
    {#if error}
      <p class="text-xs text-error-500 mt-1 break-all">{error}</p>
    {/if}
  </div>
  {#if onsync}
    <button
      type="button"
      class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center shrink-0"
      onclick={onsync}
      disabled={syncing}
      title={m.sync_status_row_sync_now()}
      aria-label={m.sync_status_row_sync_now()}
    >
      <Icon name={syncing ? 'loading' : 'sync'} size={14} />
    </button>
  {/if}
</div>
