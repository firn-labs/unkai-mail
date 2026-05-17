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

  import { invoke } from '@tauri-apps/api/core'
  import { emit } from '@tauri-apps/api/event'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import MailView from './MailView.svelte'
  import { applyTheme, installSystemModeListener, type ThemeMode } from './theme'

  // We never inspect the email shape inside the standalone window —
  // it's just a payload we forward to the main window via Tauri
  // events.  The main window's listener treats it as the existing
  // `Email` type (defined inside MailView).  Using `unknown` here
  // avoids a cross-component type-export dance.
  type EmailPayload = unknown

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
        const prefs = await invoke<{
          theme_name: string
          theme_mode: ThemeMode
          mail_html_white_background: boolean
        }>('get_app_settings')
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

  // Compose actions: emit a Tauri event the main window listens for.
  // The event payload mirrors the `Email` shape Compose's reply /
  // forward init expects, so the main window can splat it straight
  // into `openCompose`.  We don't focus the main window here — the
  // user just clicked a button in *this* window, so they know it
  // popped a Compose somewhere; jumping focus would be jarring.
  type ComposeKind = 'reply' | 'reply-all' | 'forward'
  async function emitCompose(kind: ComposeKind, mail: EmailPayload) {
    try {
      await emit('compose-from-mail', { kind, mail })
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
    void emit('respond-with-meeting-from-mail', { mail }).catch((e) => {
      console.warn('respond-with-meeting-from-mail emit failed', e)
    })
  }
  function onEditDraft(mail: EmailPayload) {
    void emit('edit-draft-from-mail', { mail }).catch((e) => {
      console.warn('edit-draft-from-mail emit failed', e)
    })
  }
  function onMailto(init: { to?: string; cc?: string; bcc?: string; subject?: string; body?: string }) {
    void emit('mailto-from-mail', { init }).catch((e) => {
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
