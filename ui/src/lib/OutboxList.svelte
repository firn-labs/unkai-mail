<script lang="ts">
  /**
   * OutboxList — local-only "send queue" view (#276).
   *
   * Mounted by App.svelte instead of MailList when the user
   * selects the synthetic "Outbox" sidebar folder.  Rows are NOT
   * IMAP envelopes — they're rows out of the local
   * `outbox_messages` table, fetched via `list_outbox` (per
   * account) or `list_all_outbox` (unified mode).  The send
   * pipeline always goes via this table now: a healthy network
   * means the row drains in the same tick as enqueue and the
   * user never sees this list; a failed send leaves the row here
   * with `last_error` populated and the periodic background
   * sweep keeps retrying.
   *
   * Per-row actions:
   *   * Retry now — `retry_outbox_entry`, kicks the drain
   *     attempt without waiting for the next sync tick.
   *   * Edit — `edit_outbox_entry` returns the queued
   *     `OutgoingEmail` and removes the row, then App.svelte
   *     re-opens Compose pre-populated.  A new send creates a
   *     fresh row.
   *   * Delete — `delete_outbox_entry`, drops the row without
   *     sending.
   *
   * Top-of-list status banner explains the queue state so the
   * user knows their mail is waiting on a retry, not lost.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'

  /** Mirror of the Rust `OutboxRowDto` — fields are camelCase
   *  on the wire (the backend uses
   *  `#[serde(rename_all = "camelCase")]`).  `outgoingJson` and
   *  `repliedToJson` are opaque on the list view; only the
   *  edit-flow consumer parses them. */
  export interface OutboxRowDto {
    id: number
    accountId: string
    fromHeader: string
    toDisplay: string
    subject: string
    queuedAt: number
    attemptCount: number
    lastAttemptAt: number | null
    lastError: string | null
    skipSentCopy: boolean
    outgoingJson: string
    repliedToJson: string | null
  }

  interface Account {
    id: string
    display_name: string
    email: string
  }

  interface Props {
    /** Active account id.  When `unified` is false, the list is
     *  filtered to this account; otherwise every account's
     *  queued rows are shown. */
    accountId: string
    /** Unified-inbox mode — list rows across every account. */
    unified?: boolean
    /** All configured accounts.  Used to render an account-label
     *  chip on each row in unified mode. */
    accounts?: Account[]
    /** Bumped by the parent (App.svelte) on `outbox-updated`
     *  events so the list re-fetches without losing its scroll
     *  position. */
    refreshToken?: number
    /** Currently-selected row id, controlled by the parent so
     *  the right pane (`OutboxView`) and the list highlight
     *  stay in sync.  `null` means no row selected — the right
     *  pane shows its empty placeholder. */
    selectedId?: number | null
    /** User clicked a row, OR the previously-selected row was
     *  refreshed / removed by a queue update.  The right-pane
     *  preview reads from this snapshot — `null` means the
     *  selected row vanished (drained successfully, deleted,
     *  or edited away) and the right pane should drop back to
     *  its empty state. */
    onselect?: (row: OutboxRowDto | null) => void
  }
  let {
    accountId,
    unified = false,
    accounts = [],
    refreshToken = 0,
    selectedId = null,
    onselect,
  }: Props = $props()

  let rows = $state<OutboxRowDto[]>([])
  let loading = $state(true)
  let error = $state('')
  let unlistenOutbox: UnlistenFn | null = null

  async function load() {
    loading = true
    error = ''
    try {
      const fresh = await invoke<OutboxRowDto[]>(
        unified ? 'list_all_outbox' : 'list_outbox',
        unified ? {} : { accountId },
      )
      rows = fresh
      // Keep the parent's preview row in sync with the fresh
      // list.  If the previously-selected id is still present,
      // re-emit the updated snapshot so attempt counts /
      // `last_error` propagate to the right-pane preview; if
      // it's gone (drained successfully, deleted, edited away),
      // clear the selection so the right pane drops back to
      // its empty state.
      if (selectedId !== null) {
        const stillThere = fresh.find((r) => r.id === selectedId) ?? null
        onselect?.(stillThere)
      }
    } catch (e) {
      error = `${e}`
    } finally {
      loading = false
    }
  }

  // Re-fetch when account / unified-mode / explicit refresh
  // signal changes.  `refreshToken` covers the parent-driven
  // bump path; the `outbox-updated` listener below covers the
  // backend-driven case.
  $effect(() => {
    refreshToken
    accountId
    unified
    void load()
  })

  $effect(() => {
    let alive = true
    void (async () => {
      try {
        const u = await listen('outbox-updated', () => {
          if (alive) void load()
        })
        unlistenOutbox = u
      } catch (e) {
        console.warn('listen outbox-updated failed', e)
      }
    })()
    return () => {
      alive = false
      unlistenOutbox?.()
      unlistenOutbox = null
    }
  })

  function formatQueuedAt(ts: number): string {
    const ms = ts * 1000
    return new Date(ms).toLocaleString()
  }

  function accountLabel(id: string): string {
    const a = accounts.find((x) => x.id === id)
    return a?.display_name || a?.email || id
  }

  function rowsHaveErrors(): boolean {
    return rows.some((r) => r.lastError)
  }
</script>

<div class="flex flex-col h-full w-full">
  <!-- Status banner (#276): always present in this view, the
       text adapts to whether anything is currently failing.
       Tells the user their mail is waiting and what the system
       is doing about it — a queue with no explanation is a
       worry; a queue with "we'll retry on the next sync" reads
       as a calm pending state. -->
  <header class="px-4 py-3 border-b border-surface-200 dark:border-surface-700 bg-surface-100 dark:bg-surface-800/60">
    <div class="flex items-start gap-2">
      <span class="text-warning-500 shrink-0 mt-0.5">
        <Icon name={rowsHaveErrors() ? 'warning' : 'time'} size={16} />
      </span>
      <div class="text-xs text-surface-700 dark:text-surface-300 leading-snug">
        {#if rows.length === 0}
          {m.outbox_banner_empty()}
        {:else if rowsHaveErrors()}
          {rows.length === 1
            ? m.outbox_banner_failed_singular()
            : m.outbox_banner_failed({ count: rows.length })}
        {:else}
          {rows.length === 1
            ? m.outbox_banner_in_flight_singular()
            : m.outbox_banner_in_flight({ count: rows.length })}
        {/if}
      </div>
    </div>
  </header>

  {#if error}
    <div class="px-4 py-2 text-xs text-error-500 border-b border-surface-200 dark:border-surface-700">
      {error}
    </div>
  {/if}

  <div class="flex-1 overflow-auto">
    {#if loading && rows.length === 0}
      <div class="px-4 py-8 text-sm text-surface-500 text-center">{m.outbox_loading()}</div>
    {:else if rows.length === 0}
      <div class="px-4 py-8 text-sm text-surface-500 text-center italic">
        {m.outbox_empty()}
      </div>
    {:else}
      {#each rows as row (row.id)}
        <div
          role="button"
          tabindex="0"
          aria-pressed={selectedId === row.id}
          class="px-4 py-3 group cursor-pointer transition-colors
            border-b border-l-[3px] border-surface-100 dark:border-surface-800
            {row.lastError ? 'border-l-error-500' : 'border-l-transparent'}
            {selectedId === row.id
              ? 'bg-primary-500/10'
              : 'hover:bg-surface-100 dark:hover:bg-surface-800'}"
          onclick={() => onselect?.(row)}
          onkeydown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault()
              onselect?.(row)
            }
          }}
        >
          <div class="flex items-center justify-between gap-2 mb-1">
            <span class="text-sm font-semibold truncate">
              {row.subject || m.outbox_no_subject()}
            </span>
            <span class="text-xs text-surface-500 shrink-0">
              {formatQueuedAt(row.queuedAt)}
            </span>
          </div>
          <div class="text-xs text-surface-500 truncate">
            {m.outbox_label_to()} {row.toDisplay || m.outbox_no_recipients()}
          </div>
          {#if unified && row.accountId}
            <div class="text-[11px] text-surface-500 mt-1 truncate">
              {m.outbox_label_from()} {accountLabel(row.accountId)}
            </div>
          {/if}
          {#if row.lastError}
            <!-- Show only the lead of the error inline so the row
                 stays compact; the full text + retry / edit /
                 delete actions live in the OutboxView preview. -->
            <div class="mt-2 text-xs text-error-500 truncate">
              <span class="font-medium">{m.outbox_label_last_error()}</span> {row.lastError}
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
</div>
