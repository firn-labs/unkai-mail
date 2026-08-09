<script lang="ts">
  /**
   * StandaloneMail — entry component for a popped-out mail window
   * (Issue #104).  Wraps `MailView` with no sidebar / mail-list
   * chrome so the message gets the entire window, and routes
   * Reply / Reply All / Forward through Tauri events so the
   * existing Compose flow runs in the *main* window (where
   * autocomplete state, signatures, draft folder lookup all live)
   * without us having to re-implement Compose here.
   *
   * Archive / Delete / Edit-draft close this window after
   * completing — once the message is gone there's nothing left
   * for the standalone reader to show.
   *
   * URL contract: `?view=mail&account=<id>&folder=<name>&uid=<n>`
   * is set by `openMailInStandaloneWindow` in the helper module.
   * `main.ts` routes the query into our props.
   */

  import * as api from './api'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import MailView from './MailView.svelte'
  import PasswordInput from './PasswordInput.svelte'
  import { applyTheme, installSystemModeListener } from './theme'
  import { formatError } from './errors'
  import { m } from '../paraglide/messages'

  // We never inspect the email shape inside the standalone window —
  // it's just a payload we forward to the main window via Tauri
  // events.  The main window's listener treats it as the existing
  // `Email` type (defined inside MailView).  Using `unknown` here
  // avoids a cross-component type-export dance; the decrypt path
  // below narrows the few fields it needs via an internal alias.
  type EmailPayload = unknown

  /** #341 — subset of MailView's `Email` we actually inspect inside
   *  `emitCompose` to decide whether the popout needs to prompt for
   *  a passphrase before forwarding the payload on.  Narrowed via
   *  an `as` cast at the boundary so the rest of this file stays
   *  on the opaque `EmailPayload` type.  `attachments` is included
   *  because Forward of an encrypted message with attachments needs
   *  the passphrase even if the body is already in the cache —
   *  attachment bytes only come out of the inner MIME tree via
   *  `download_decrypted_attachment`. */
  type DecryptableMail = {
    account_id: string
    folder: string
    uid: number
    from: string
    body_text: string | null
    body_html?: string | null
    protection?: string | null
    attachments?: { part_id: number }[]
  }

  let {
    accountId,
    folder = 'INBOX',
    uid,
  }: {
    accountId: string
    folder?: string
    uid: number
  } = $props()

  // Mirror the main app's preferences so the standalone reader picks
  // up the user's chosen Skeleton theme + light/dark mode + the
  // white-canvas preference instead of falling back to defaults.
  // Best-effort — if `get_app_settings` fails (race on first launch,
  // backend hiccup) we keep the defaults already on `<html>`.
  let forceWhiteBackground = $state(true)
  $effect(() => {
    let unlistenSystem: (() => void) | null = null
    void (async () => {
      try {
        const prefs = await api.settings.getAppSettings()
        forceWhiteBackground = prefs.mail_html_white_background ?? true
        applyTheme(prefs.theme_name, prefs.theme_mode)
        unlistenSystem = installSystemModeListener(
          prefs.theme_mode,
          prefs.theme_name,
        )
      } catch (e) {
        console.warn('get_app_settings failed in standalone window', e)
      }
    })()
    return () => {
      unlistenSystem?.()
    }
  })

  // #341 — Reply / Forward on an encrypted-but-not-yet-decrypted
  // message needs the user's PGP passphrase so we can decrypt the
  // body (for the quoted-history block) and any attachments (for
  // the forward fan-out).  Owning the prompt locally — rather than
  // letting the main-window listener prompt — keeps the modal next
  // to the popped-out mail the user just clicked Reply on, instead
  // of stealing focus over to a main window the user may have
  // pushed to a different monitor.  After collecting the passphrase
  // we decrypt locally via `decrypt_message` and emit
  // `compose-from-mail` with the already-decrypted payload + the
  // passphrase, so the main window can hand it through to the
  // forward-attachment fetch without re-prompting.
  type ComposeKind = 'reply' | 'reply-all' | 'forward'
  type PendingPrompt = {
    kind: ComposeKind
    fromName: string
    resolve: (passphrase: string | null) => void
    error: string
    busy: boolean
    value: string
  }
  let pendingDecryptPrompt = $state<PendingPrompt | null>(null)

  // #341 — same window-locality rule for the "include original
  // attachments?" prompt (#329).  Previously this fired from the
  // main window's `buildForwardInitialForPopout`, so the user
  // clicked Forward in the popout, answered a passphrase prompt
  // here, then had to switch monitors to find the include-or-not
  // modal in the main window.  Owning it locally too means a
  // popped-out forward never bounces focus until the resulting
  // Compose actually opens.
  let pendingIncludeAttachmentsPrompt = $state<{
    count: number
    resolve: (include: boolean) => void
  } | null>(null)

  function resolveIncludeAttachmentsPrompt(include: boolean) {
    if (!pendingIncludeAttachmentsPrompt) return
    const { resolve } = pendingIncludeAttachmentsPrompt
    pendingIncludeAttachmentsPrompt = null
    resolve(include)
  }

  function promptIncludeAttachments(count: number): Promise<boolean> {
    return new Promise((resolve) => {
      pendingIncludeAttachmentsPrompt = { count, resolve }
    })
  }

  function resolveDecryptPrompt(passphrase: string | null) {
    if (!pendingDecryptPrompt) return
    if (pendingDecryptPrompt.busy) return
    const { resolve } = pendingDecryptPrompt
    pendingDecryptPrompt = null
    resolve(passphrase)
  }

  /** Was the source PGP-encrypted by the receive path?  Both
   *  encrypt-only and the signed-and-encrypted variants qualify. */
  function wasEncrypted(mail: DecryptableMail): boolean {
    const p = mail.protection
    return p === 'encrypted' || p === 'signed-and-encrypted'
  }

  /** Does this mail need the popout-side decrypt step?  Same logic
   *  as App.svelte's `needsDecryptForReply` — encrypted protection
   *  tag *and* an empty cached body.  When the user clicked Decrypt
   *  inside the popout's reading pane already, the cache row
   *  carries the plaintext and this short-circuits to false. */
  function needsBodyDecrypt(mail: DecryptableMail): boolean {
    if (!wasEncrypted(mail)) return false
    const text = (mail.body_text ?? '').trim()
    const html = (mail.body_html ?? '').trim()
    return text.length === 0 && html.length === 0
  }

  /** Does the popout-local prompt need to fire for this Reply /
   *  Forward?  Two triggers, matched to App.svelte:
   *    - Body needs decrypting (cached body is empty).
   *    - Forward of an encrypted message with attachments — the
   *      passphrase rides through to `downloadForwardAttachments`
   *      so the bytes come from the inner MIME tree instead of the
   *      outer `multipart/encrypted` envelope.  Without this, a
   *      forward triggered after the user clicked Decrypt in the
   *      popout would still ship "Version: 1" header bytes. */
  function needsPromptForKind(mail: DecryptableMail, kind: ComposeKind): boolean {
    if (needsBodyDecrypt(mail)) return true
    if (kind === 'forward' && wasEncrypted(mail)) {
      return (mail.attachments?.length ?? 0) > 0
    }
    return false
  }

  /** Prompt for the passphrase (looping on wrong input), call
   *  `decrypt_message`, return `{ mail, passphrase }` with the
   *  plaintext bodies overlaid.  Returns `null` if the user
   *  cancels — caller aborts the emit so no compose-from-mail
   *  event reaches the main window.  Pass-through (no prompt,
   *  passphrase null) when the mail doesn't need decryption. */
  async function ensureDecryptedLocal(
    mail: EmailPayload,
    kind: ComposeKind,
  ): Promise<{ mail: EmailPayload; passphrase: string | null } | null> {
    // Narrow once at the boundary so the rest of this function can
    // touch decrypt-relevant fields directly without further casts.
    // The cast is safe in practice because every caller path threads
    // a MailView `Email & { uid }` here; the narrower type lists the
    // subset we actually read.
    const narrow = mail as DecryptableMail
    if (!needsPromptForKind(narrow, kind)) return { mail, passphrase: null }
    // #341 — same auto-decrypt-first pattern as App.svelte's
    // `ensureDecryptedForReply`.  Backend returns `Ok(None)` without
    // any IMAP work when the account hasn't opted in, so opt-out
    // users fall through to the manual prompt below with no
    // visible delay.  Returning `passphrase: ''` rather than the
    // typed value tells the main-window listener (and
    // `downloadForwardAttachments`) to route attachment fetches
    // through the keychain via `resolve_pgp_passphrase`.
    let lastError = ''
    try {
      const auto = await api.crypto.tryAutoDecryptMessage({
        accountId: narrow.account_id,
        folder: narrow.folder,
        uid: narrow.uid,
      })
      if (auto) {
        return {
          mail: {
            ...(mail as object),
            body_text: auto.body_text,
            body_html: auto.body_html,
            attachments: auto.attachments,
          } as EmailPayload,
          passphrase: '',
        }
      }
    } catch (e) {
      const raw = formatError(e) || 'Decrypt failed'
      lastError = raw.replace(/^Crypto:\s*/i, '')
    }
    while (true) {
      const passphrase = await new Promise<string | null>((resolve) => {
        pendingDecryptPrompt = {
          kind,
          fromName: narrow.from,
          resolve,
          error: lastError,
          busy: false,
          value: '',
        }
      })
      if (passphrase == null) {
        pendingDecryptPrompt = null
        return null
      }
      pendingDecryptPrompt = {
        kind,
        fromName: narrow.from,
        resolve: () => {},
        error: '',
        busy: true,
        value: passphrase,
      }
      try {
        // Pull `attachments` back too — `decrypt_message` returns
        // the full `Email`, and the freshly-decrypted attachments
        // list indexes the *inner* MIME tree (real files), while
        // the cached envelope the user clicked Forward on still
        // lists the *outer* `multipart/encrypted` parts (a
        // `Version: 1` header part + the armored octet-stream).
        // Overlaying both bodies AND the attachments here means
        // the main-window listener can route the forward fan-out
        // through `download_decrypted_attachment` with valid
        // inner-tree part_ids.
        const decrypted = await api.crypto.decryptMessage({
          accountId: narrow.account_id,
          folder: narrow.folder,
          uid: narrow.uid,
          pgpPassphrase: passphrase,
        })
        pendingDecryptPrompt = null
        return {
          mail: {
            ...(mail as object),
            body_text: decrypted.body_text,
            body_html: decrypted.body_html,
            attachments: decrypted.attachments,
          } as EmailPayload,
          passphrase,
        }
      } catch (e) {
        const raw = formatError(e) || 'Decrypt failed'
        lastError = raw.replace(/^Crypto:\s*/i, '')
      }
    }
  }

  // Compose actions: emit a Tauri event the main window listens for.
  // The event payload mirrors the `Email` shape Compose's reply /
  // forward init expects, so the main window can splat it straight
  // into `openCompose`.  We don't focus the main window here — the
  // user just clicked a button in *this* window, so they know it
  // popped a Compose somewhere; jumping focus would be jarring.
  async function emitCompose(kind: ComposeKind, mail: EmailPayload) {
    try {
      const ready = await ensureDecryptedLocal(mail, kind)
      if (!ready) return
      // #341 — for a forward that carries attachments, ask the
      // include/skip question *here* so the modal sits next to the
      // popout the user just clicked Forward on.  Default
      // ("Forward without" on backdrop / Escape dismiss) matches
      // the in-window flow.  Reply / Reply All never propagate the
      // source attachments, so we skip the prompt for those kinds.
      // `null` for `includeAttachments` is the "no question
      // needed" signal so the main window can route accordingly
      // without re-prompting.
      let includeAttachments: boolean | null = null
      if (kind === 'forward') {
        const narrow = ready.mail as DecryptableMail
        const count = narrow.attachments?.length ?? 0
        if (count > 0) {
          includeAttachments = await promptIncludeAttachments(count)
        }
      }
      await api.emitAppEvent('compose-from-mail', {
        kind,
        mail: ready.mail,
        pgpPassphrase: ready.passphrase,
        includeAttachments,
      })
    } catch (e) {
      console.warn(`compose-from-mail (${kind}) emit failed`, e)
    }
  }

  function onReply(mail: EmailPayload) {
    void emitCompose('reply', mail)
  }
  function onReplyAll(mail: EmailPayload) {
    void emitCompose('reply-all', mail)
  }
  function onForward(mail: EmailPayload) {
    void emitCompose('forward', mail)
  }
  // #304 — "Respond with meeting" from a popped-out mail.  The
  // EventEditor lives in the main window (app-level surface), so
  // we just forward the message and let App.svelte open the
  // editor and remember that the trigger came from a popout —
  // the resulting Compose ends up as its own popped-out window.
  function onRespondWithMeeting(mail: EmailPayload) {
    void api.emitAppEvent('respond-with-meeting-from-mail', { mail }).catch((e) => {
      console.warn('respond-with-meeting-from-mail emit failed', e)
    })
  }
  function onEditDraft(mail: EmailPayload) {
    void api.emitAppEvent('edit-draft-from-mail', { mail }).catch((e) => {
      console.warn('edit-draft-from-mail emit failed', e)
    })
  }
  function onMailto(init: { to?: string; cc?: string; bcc?: string; subject?: string; body?: string }) {
    void api.emitAppEvent('mailto-from-mail', { init }).catch((e) => {
      console.warn('mailto-from-mail emit failed', e)
    })
  }

  function closeWindow() {
    void getCurrentWindow().close()
  }
</script>

<div class="h-screen flex flex-col bg-surface-50 dark:bg-surface-900">
  <MailView
    {accountId}
    {folder}
    {uid}
    {forceWhiteBackground}
    inStandaloneWindow={true}
    onreply={onReply}
    onreplyall={onReplyAll}
    onforward={onForward}
    onrespondwithmeeting={onRespondWithMeeting}
    oneditdraft={onEditDraft}
    onmessageremoved={closeWindow}
    onmailto={onMailto}
  />
</div>

<!-- #341 — popout-local "include original attachments?" prompt.
     Same shape App.svelte's `pendingForwardPrompt` modal uses, just
     instantiated here so a popout-originated Forward keeps the
     follow-up question in the popout window too. -->
{#if pendingIncludeAttachmentsPrompt}
  <div
    class="fixed inset-0 z-60 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    aria-labelledby="forward-attachments-title-popout"
    tabindex="-1"
    onclick={(e) => {
      if (e.target === e.currentTarget) resolveIncludeAttachmentsPrompt(false)
    }}
    onkeydown={(e) => e.key === 'Escape' && resolveIncludeAttachmentsPrompt(false)}
  >
    <div
      class="card p-5 max-w-sm w-[90%] glass-float rounded-2xl"
    >
      <h2 id="forward-attachments-title-popout" class="text-base font-semibold mb-2">
        {m.compose_forward_attachments_title()}
      </h2>
      <p class="text-sm text-surface-600 dark:text-surface-300 mb-4">
        {pendingIncludeAttachmentsPrompt.count === 1
          ? m.compose_forward_attachments_body_one()
          : m.compose_forward_attachments_body_many({
              n: pendingIncludeAttachmentsPrompt.count,
            })}
      </p>
      <div class="flex justify-end gap-2">
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500"
          onclick={() => resolveIncludeAttachmentsPrompt(false)}
        >
          {m.compose_forward_attachments_skip()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-filled-primary-500"
          onclick={() => resolveIncludeAttachmentsPrompt(true)}
        >
          {m.compose_forward_attachments_include()}
        </button>
      </div>
    </div>
  </div>
{/if}

<!-- #341 — popout-local decrypt prompt.  Same shape App.svelte
     mounts at the main-window level, just instantiated here so it
     appears on the same screen as the popped-out mail the user
     just clicked Reply / Forward on. -->
{#if pendingDecryptPrompt}
  <div
    class="fixed inset-0 z-60 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    aria-labelledby="decrypt-prompt-title"
    tabindex="-1"
    onclick={(e) => {
      if (e.target === e.currentTarget) resolveDecryptPrompt(null)
    }}
    onkeydown={(e) => e.key === 'Escape' && resolveDecryptPrompt(null)}
  >
    <div
      class="card p-5 max-w-sm w-[90%] glass-float rounded-2xl"
    >
      <h2 id="decrypt-prompt-title" class="text-base font-semibold mb-2">
        {pendingDecryptPrompt.kind === 'forward'
          ? m.mail_decrypt_for_forward_title()
          : m.mail_decrypt_for_reply_title()}
      </h2>
      <p class="text-sm text-surface-600 dark:text-surface-300 mb-3">
        {pendingDecryptPrompt.kind === 'forward'
          ? m.mail_decrypt_for_forward_body({
              sender: pendingDecryptPrompt.fromName,
            })
          : m.mail_decrypt_for_reply_body({
              sender: pendingDecryptPrompt.fromName,
            })}
      </p>
      <form
        onsubmit={(e) => {
          e.preventDefault()
          if (!pendingDecryptPrompt || pendingDecryptPrompt.busy) return
          resolveDecryptPrompt(pendingDecryptPrompt.value)
        }}
      >
        <label
          for="decrypt-prompt-passphrase-popout"
          class="block text-xs text-surface-500 mb-1"
        >
          {m.mail_decrypt_passphrase_label()}
        </label>
        <PasswordInput
          id="decrypt-prompt-passphrase-popout"
          class="mb-2"
          inputClass="px-3 py-2 text-sm rounded-lg"
          bind:value={pendingDecryptPrompt.value}
          disabled={pendingDecryptPrompt.busy}
          autofocus={!pendingDecryptPrompt.busy}
        />
        {#if pendingDecryptPrompt.error}
          <p class="text-xs text-red-500 mb-3">{pendingDecryptPrompt.error}</p>
        {/if}
        <div class="flex justify-end gap-2 mt-2">
          <button
            type="button"
            class="btn btn-sm preset-outlined-surface-500"
            disabled={pendingDecryptPrompt.busy}
            onclick={() => resolveDecryptPrompt(null)}
          >
            {m.mail_decrypt_cancel_button()}
          </button>
          <button
            type="submit"
            class="btn btn-sm preset-filled-primary-500"
            disabled={pendingDecryptPrompt.busy || !pendingDecryptPrompt.value}
          >
            {pendingDecryptPrompt.busy
              ? m.mail_decrypt_busy_button()
              : m.mail_decrypt_submit_button()}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
