<script lang="ts">
  /**
   * OutboxView — right-pane preview for a queued Outbox row
   * (#276 follow-up).
   *
   * Mounted by App.svelte when the user clicks an OutboxList
   * row.  Renders the queued message's headers + body so the
   * user can see what's about to be sent without opening
   * Compose.  Read-only — the actions (retry / edit / delete)
   * still live on the OutboxList row's hover cluster, this
   * surface is purely "show me what's in here".
   *
   * Body rendering parallels MailView's: prefer the saved HTML
   * body and run it through DOMPurify before injection;
   * otherwise fall back to plain text wrapped in a `<pre>` for
   * whitespace fidelity.  The content is the user's own
   * outgoing draft so we don't need MailView's full per-sender
   * trust / image-blocking dance — but we still sanitise to
   * keep a malformed local DB from injecting script.
   */

  import * as api from './api'
  import DOMPurify from 'dompurify'
  import Icon from './Icon.svelte'
  import PasswordInput from './PasswordInput.svelte'
  import { m } from '../paraglide/messages'
  import type { OutboxRowDto } from './OutboxList.svelte'

  interface Props {
    row: OutboxRowDto
    /** Re-open this queued message in Compose for editing.  Same
     *  callback shape OutboxList used to forward; App.svelte
     *  deserialises the row's `OutgoingEmail`, opens Compose
     *  with `outboxSource: { id }` set so a successful send
     *  replaces the queued copy. */
    onedit: (row: OutboxRowDto) => void
  }
  let { row, onedit }: Props = $props()

  /** True while a retry / delete invoke is in flight against
   *  this row.  Disables both buttons so a user with a slow
   *  network can't kick off a flurry of duplicate retries.  */
  let busy = $state(false)
  let actionError = $state('')

  /** Inline passphrase prompt state for the encrypted-retry UX
   *  (#341).  `passphrasePanelOpen` is the toggle that reveals the
   *  panel under the action cluster; `passphraseInput` is the
   *  bound value (typed locally — only flows out via the invoke
   *  argument, never stored).  `passphraseError` is the inline
   *  status line so the user can re-type after a wrong-passphrase
   *  attempt without losing the panel.  Cleared on every fresh
   *  submit so a stale error never paints alongside a new attempt.
   *  We track the row id we opened on so we can collapse the panel
   *  automatically when the user clicks a different row (the
   *  parent OutboxView is re-used across rows; without this guard
   *  the panel would carry between rows). */
  let passphrasePanelOpen = $state(false)
  let passphraseInput = $state('')
  let passphraseError = $state('')
  let passphrasePanelRowId = $state<number | null>(null)

  $effect(() => {
    // Collapse + scrub the panel whenever the selected row
    // changes.  Reading `row.id` ties the effect to the row swap;
    // the password value never lingers across rows.
    if (passphrasePanelRowId !== null && passphrasePanelRowId !== row.id) {
      passphrasePanelOpen = false
      passphraseInput = ''
      passphraseError = ''
      passphrasePanelRowId = null
    }
  })

  /** Auto-unlock fast path (#341, #355).  When the account this
   *  row belongs to has the per-account "Unlock automatically"
   *  toggle on, the backend's send pipeline resolves the
   *  passphrase from the OS keychain — no prompt needed.  We
   *  pre-check the toggle, and if it's on we just submit an empty
   *  passphrase to the awaiting retry command and let the backend
   *  precedence (caller -> keychain) handle the rest.  The check
   *  is per-row (the user may have a mix of opted-in and opted-out
   *  accounts), so we re-fetch on every retry click rather than
   *  caching the answer.  Falls back to the prompt path on any
   *  IPC error rather than blocking the user. */
  async function accountHasAutoUnlock(): Promise<boolean> {
    try {
      return await api.crypto.pgpHasUnlockAutomatically({
        accountId: row.accountId,
      })
    } catch (e) {
      console.warn('pgp_has_unlock_automatically lookup failed', e)
      return false
    }
  }

  async function retry() {
    if (busy) return
    // For PGP-active rows that previously failed, open the
    // passphrase panel (or skip it on the auto-unlock fast path)
    // instead of firing the no-passphrase retry that the
    // background sweep already runs and that already failed.
    if (pgpActive && row.lastError) {
      if (await accountHasAutoUnlock()) {
        await submitWithPassphrase('')
        return
      }
      passphrasePanelOpen = true
      passphrasePanelRowId = row.id
      passphraseError = ''
      return
    }
    busy = true
    actionError = ''
    try {
      await api.compose.retryOutboxEntry({ id: row.id })
    } catch (e) {
      actionError = `${e}`
    } finally {
      busy = false
    }
  }

  /** Drive the awaiting backend command + map the result into the
   *  inline error / collapsed-panel transitions.  Used by both the
   *  auto-unlock fast path (empty `passphrase`) and the user-typed
   *  submit.  The success branch leaves cleanup to the
   *  `outbox-updated` event — the OutboxList listener re-fetches,
   *  the row vanishes, and the parent passes `null` back through
   *  `onselect` which unmounts this whole view. */
  async function submitWithPassphrase(passphrase: string) {
    busy = true
    actionError = ''
    passphraseError = ''
    try {
      await api.compose.retryOutboxEntryWithPassphrase({
        id: row.id,
        pgpPassphrase: passphrase,
      })
      // Successful drain — clear the typed value and let the
      // outbox-updated event collapse the surface around us.
      passphraseInput = ''
      passphrasePanelOpen = false
      passphrasePanelRowId = null
    } catch (e) {
      // Stay on the panel so the user can re-type without
      // re-finding it.  We surface the raw error string —
      // "Crypto error: …" for a wrong passphrase, "Network
      // error: …" for transport, etc. — so the user can tell a
      // typo from a server problem.
      passphraseError = `${e}`
    } finally {
      busy = false
    }
  }

  async function discard() {
    // Cheap confirm — Outbox deletion drops the user's mail
    // before SMTP, so a misclick is destructive.
    const subject = row.subject || m.outbox_no_subject()
    if (!confirm(m.outbox_confirm_discard({ subject }))) {
      return
    }
    if (busy) return
    busy = true
    actionError = ''
    try {
      await api.compose.deleteOutboxEntry({ id: row.id })
    } catch (e) {
      actionError = `${e}`
    } finally {
      busy = false
    }
  }

  function edit() {
    onedit(row)
  }

  /** Parsed `OutgoingEmail` (the JSON-serialised one stored in
   *  the outbox table).  We re-deserialise on every row change
   *  so a `Retry now` that bumps `attempt_count` triggers a
   *  fresh paint of the same content.  Cheap — JSON.parse on a
   *  few KB. */
  interface ParsedOutgoing {
    from: string
    to: string[]
    cc: string[]
    bcc: string[]
    reply_to: string | null
    subject: string
    body_text: string | null
    body_html: string | null
    attachments: Array<{ filename: string; content_type: string }>
    /** Outgoing `OutgoingEmail.encryption_mode` (#57).  `"pgp"`
     *  means the row asked for PGP/MIME encryption; anything else
     *  (`null`, `"none"`) is plaintext from the encryption side. */
    encryption_mode?: string | null
    /** Outgoing `OutgoingEmail.signing_enabled` (#341 Part 7).
     *  `true` for the sign-only path which also needs the private
     *  key unlocked — same passphrase precedence as encryption. */
    signing_enabled?: boolean
  }

  let parsed = $derived.by<ParsedOutgoing | null>(() => {
    try {
      return JSON.parse(row.outgoingJson) as ParsedOutgoing
    } catch {
      return null
    }
  })

  function sanitiseHtml(html: string): string {
    return DOMPurify.sanitize(html, {
      FORBID_TAGS: [
        'script',
        'noscript',
        'object',
        'embed',
        'applet',
        'iframe',
        'frame',
        'frameset',
        'form',
        'base',
        'meta',
        'link',
        'style',
      ],
      ADD_ATTR: ['target', 'title'],
      FORCE_BODY: true,
    })
  }

  let renderedHtml = $derived(
    parsed && parsed.body_html ? sanitiseHtml(parsed.body_html) : '',
  )

  /** True when this queued message will exercise the PGP private
   *  key on send — either full `multipart/encrypted` (encryption
   *  mode "pgp") or sign-only (#341 Part 7).  Both paths need the
   *  same passphrase prompt UX on a failed retry; plaintext rows
   *  fall back to the standard fire-and-forget Retry. */
  let pgpActive = $derived(
    parsed
      ? parsed.encryption_mode === 'pgp' || parsed.signing_enabled === true
      : false,
  )

  function formatTimestamp(unixSec: number): string {
    return new Date(unixSec * 1000).toLocaleString()
  }
</script>

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900 text-surface-900 dark:text-surface-100 overflow-hidden">
  {#if !parsed}
    <div class="flex-1 flex items-center justify-center text-sm text-surface-500 px-6 text-center">
      {m.outbox_view_unreadable()}
    </div>
  {:else}
    <header class="px-6 pt-5 pb-4 border-b border-surface-300/60 dark:border-surface-700/60">
      <h1 class="text-lg font-semibold leading-snug wrap-break-word">
        {parsed.subject || m.outbox_no_subject()}
      </h1>
      <dl class="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        <dt class="text-surface-500">{m.outbox_view_label_from()}</dt>
        <dd class="wrap-break-word">{parsed.from || row.fromHeader}</dd>
        {#if parsed.to.length > 0}
          <dt class="text-surface-500">{m.outbox_view_label_to()}</dt>
          <dd class="wrap-break-word">{parsed.to.join(', ')}</dd>
        {/if}
        {#if parsed.cc.length > 0}
          <dt class="text-surface-500">{m.outbox_view_label_cc()}</dt>
          <dd class="wrap-break-word">{parsed.cc.join(', ')}</dd>
        {/if}
        {#if parsed.bcc.length > 0}
          <dt class="text-surface-500">{m.outbox_view_label_bcc()}</dt>
          <dd class="wrap-break-word">{parsed.bcc.join(', ')}</dd>
        {/if}
        <dt class="text-surface-500">{m.outbox_view_label_queued_at()}</dt>
        <dd>{formatTimestamp(row.queuedAt)}</dd>
        {#if row.attemptCount > 0}
          <dt class="text-surface-500">{m.outbox_view_label_attempts()}</dt>
          <dd>{row.attemptCount}</dd>
        {/if}
      </dl>

      {#if row.lastError}
        <div class="mt-3 flex items-start gap-2 text-xs text-error-500">
          <span class="shrink-0 mt-0.5"><Icon name="warning" size={14} /></span>
          <div class="wrap-break-word">
            <span class="font-medium">{m.outbox_label_last_error()}</span>
            {row.lastError}
          </div>
        </div>
      {/if}

      <!-- Action cluster sits under the error block so the user
           reads "this couldn't send because X — here's what you
           can do about it" as one connected thought.  When the
           row hasn't failed yet (`row.lastError` is null) the
           cluster sits directly under the metadata grid above —
           still useful for forcing an early retry or editing a
           message that's quietly stuck mid-flight. -->
      <div class="mt-3 flex flex-wrap items-center gap-1.5">
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5"
          disabled={busy}
          onclick={() => void retry()}
          title={pgpActive && row.lastError
            ? m.outbox_button_retry_with_passphrase_title()
            : m.outbox_button_retry_title()}
        >
          <Icon name={busy ? 'loading' : pgpActive && row.lastError ? 'lock' : 'sync'} size={14} />
          {pgpActive && row.lastError
            ? m.outbox_button_retry_with_passphrase()
            : m.outbox_button_retry()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5"
          onclick={edit}
          title={m.outbox_button_edit_title()}
        >
          <Icon name="compose" size={14} />
          {m.outbox_button_edit()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5 hover:bg-red-500/15 hover:text-red-500"
          disabled={busy}
          onclick={() => void discard()}
          title={m.outbox_button_delete_title()}
        >
          <Icon name="trash" size={14} />
          {m.outbox_button_delete()}
        </button>
      </div>

      {#if actionError}
        <div class="mt-2 text-xs text-error-500 wrap-break-word">
          {actionError}
        </div>
      {/if}

      <!-- Inline passphrase panel for the encrypted-retry UX
           (#341).  Mirrors MailView's decrypt-passphrase panel —
           explanation copy on top, input + submit on a second
           row — so the two encryption-passphrase surfaces share a
           visual language.  Hidden by default; the Retry button
           toggles it on for PGP-active rows that have a previous
           failure (rows that haven't tried yet still get the
           plain fire-and-forget retry). -->
      {#if passphrasePanelOpen}
        <div
          class="mt-3 rounded-lg border border-primary-300 bg-primary-50 dark:border-primary-700 dark:bg-primary-900/30 p-3 flex flex-col gap-2"
        >
          <p class="text-sm text-surface-600 dark:text-surface-400">
            {m.outbox_passphrase_explain()}
          </p>
          <div class="flex items-center gap-2">
            <PasswordInput
              class="flex-1"
              inputClass="text-sm px-2 py-1.5 rounded-lg"
              placeholder={m.outbox_passphrase_input_placeholder()}
              bind:value={passphraseInput}
              disabled={busy}
              onkeydown={(e) => {
                if (e.key === 'Enter' && passphraseInput) {
                  void submitWithPassphrase(passphraseInput)
                } else if (e.key === 'Escape') {
                  passphrasePanelOpen = false
                  passphraseInput = ''
                  passphraseError = ''
                }
              }}
            />
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center gap-1.5 hover:border-surface-50 shrink-0"
              disabled={busy || !passphraseInput}
              onclick={() => void submitWithPassphrase(passphraseInput)}
              title={m.outbox_button_retry_with_passphrase_title()}
            >
              {busy ? m.outbox_passphrase_submit_busy() : m.outbox_passphrase_submit()}
            </button>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center shrink-0"
              disabled={busy}
              onclick={() => {
                passphrasePanelOpen = false
                passphraseInput = ''
                passphraseError = ''
              }}
              title={m.outbox_passphrase_cancel()}
            >
              {m.outbox_passphrase_cancel()}
            </button>
          </div>
          {#if passphraseError}
            <div class="text-xs text-error-500 wrap-break-word">
              {passphraseError}
            </div>
          {/if}
        </div>
      {/if}

      {#if parsed.attachments && parsed.attachments.length > 0}
        <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-surface-500">
          <span class="flex items-center gap-1">
            <Icon name="attachment" size={12} />
            {parsed.attachments.length}
          </span>
          {#each parsed.attachments as a}
            <span class="px-2 py-0.5 rounded-full bg-surface-200 dark:bg-surface-800 wrap-break-word">
              {a.filename || a.content_type}
            </span>
          {/each}
        </div>
      {/if}
    </header>

    <section class="flex-1 overflow-auto">
      {#if renderedHtml}
        <div class="px-6 py-4 outbox-body" style="background:white;color:#111">
          {@html renderedHtml}
        </div>
      {:else if parsed.body_text}
        <pre class="px-6 py-4 text-sm whitespace-pre-wrap wrap-break-word font-mono">{parsed.body_text}</pre>
      {:else}
        <div class="px-6 py-4 text-sm text-surface-500 italic">
          {m.outbox_view_empty_body()}
        </div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .outbox-body :global(img) {
    max-width: 100%;
    height: auto;
  }
  .outbox-body :global(table) {
    max-width: 100%;
  }
</style>
