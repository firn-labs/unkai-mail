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
    /** Re-open this queued message in Compose for editing.
     *  App.svelte calls `edit_outbox_entry` (which removes the
     *  row) and pipes the result into a fresh Compose modal
     *  with the original recipients / body / answered-tracking
     *  ref intact. */
    onedit: (row: OutboxRowDto) => void
  }
  let {
    accountId,
    unified = false,
    accounts = [],
    refreshToken = 0,
    onedit,
  }: Props = $props()

  let rows = $state<OutboxRowDto[]>([])
  let loading = $state(true)
  let error = $state('')
  let busyId = $state<number | null>(null)
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

  async function retryRow(row: OutboxRowDto) {
    if (busyId !== null) return
    busyId = row.id
    try {
      await invoke('retry_outbox_entry', { id: row.id })
    } catch (e) {
      error = `${e}`
    } finally {
      busyId = null
    }
  }

  async function deleteRow(row: OutboxRowDto) {
    // Cheap confirmation dialog — Outbox deletion drops the
    // user's mail before SMTP, so a misclick is destructive.
    const subject = row.subject || m.outbox_no_subject()
    if (!confirm(m.outbox_confirm_discard({ subject }))) {
      return
    }
    if (busyId !== null) return
    busyId = row.id
    try {
      await invoke('delete_outbox_entry', { id: row.id })
    } catch (e) {
      error = `${e}`
    } finally {
      busyId = null
    }
  }

  function editRow(row: OutboxRowDto) {
    // The actual `edit_outbox_entry` invoke happens in App.svelte
    // so it can both remove the row AND re-open Compose with the
    // returned payload as one atomic gesture.  Passing the row
    // (with its id) is enough — App.svelte uses the id to call
    // the backend.
    onedit(row)
  }

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
          class="border-b border-surface-100 dark:border-surface-800 px-4 py-3 group hover:bg-surface-50 dark:hover:bg-surface-900/40"
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
            <div class="mt-2 text-xs text-error-500 wrap-break-word">
              <span class="font-medium">{m.outbox_label_last_error()}</span> {row.lastError}
              {#if row.attemptCount > 0}
                <span class="text-surface-500"> · {row.attemptCount === 1
                  ? m.outbox_label_attempts_singular()
                  : m.outbox_label_attempts({ count: row.attemptCount })}</span>
              {/if}
            </div>
          {/if}
          <div class="mt-2 flex items-center gap-1.5">
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5"
              disabled={busyId === row.id}
              onclick={() => void retryRow(row)}
              title={m.outbox_button_retry_title()}
            >
              <Icon name={busyId === row.id ? 'loading' : 'sync'} size={14} />
              {m.outbox_button_retry()}
            </button>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5"
              onclick={() => editRow(row)}
              title={m.outbox_button_edit_title()}
            >
              <Icon name="compose" size={14} />
              {m.outbox_button_edit()}
            </button>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5 hover:bg-red-500/15 hover:text-red-500"
              disabled={busyId === row.id}
              onclick={() => void deleteRow(row)}
              title={m.outbox_button_delete_title()}
            >
              <Icon name="trash" size={14} />
              {m.outbox_button_delete()}
            </button>
          </div>
        </div>
      {/each}
    {/if}
  </div>
</div>
