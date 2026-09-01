<script lang="ts">
  /**
   * StandaloneSignatureEditor — entry component for a popped-out
   * signature editor window (#314).  Takes a key from the URL, reads
   * the launch payload (account id + initial HTML) stashed in
   * localStorage by the launcher, fetches the full account row, then
   * mounts the same `RichTextEditor` the settings panel uses, this
   * time filling the whole window so the user gets room to author a
   * long / image-heavy signature.
   *
   * Save flow is auto-save: every keystroke (debounced) writes
   * `update_account` and fires `signature-updated-from-popout` so
   * the main window's AccountSettings can sync its in-memory copy
   * without re-fetching every account.  When the window closes
   * (OS button, in-window Done, or `tauri://close-requested` for
   * any other reason) we flush any pending debounced save and emit
   * `signature-popout-closed` so the main window unlocks its inline
   * editor for this account.
   */

  import * as api from './api'
  import type { Account } from './api'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import RichTextEditor from './RichTextEditor.svelte'
  import {
    SIGNATURE_POPOUT_CLOSED_EVENT,
    SIGNATURE_UPDATED_EVENT,
    takeSignatureEditorPopoutPayload,
  } from './standaloneSignatureEditorWindow'
  import { applyTheme, installSystemModeListener } from './theme'
  import { m } from '../paraglide/messages'

  let { popoutKey }: { popoutKey: string } = $props()

  // The full account row (typed for real in `api/types.ts` since
  // #534).  The popout never touches host / port / passwords; it
  // round-trips whatever the backend hands back so the Rust side's
  // `Account` record stays whole.
  let account = $state<Account | null>(null)
  let html = $state('')
  let saveStatus = $state<'' | 'saving' | 'saved' | 'error'>('')
  let loadError = $state('')

  // Debounced save — mirrors the 400ms debounce of the inline
  // editor in AccountSettings so the two surfaces feel identical.
  let saveTimer: ReturnType<typeof setTimeout> | null = null
  // Latest HTML the user has typed.  Held separately from the
  // `html` state binding so the close-flush always sends the most
  // recent value even if the debounced timer hasn't fired yet.
  let pendingHtml: string | null = null

  async function persist(next: string) {
    if (!account) return
    try {
      await api.accounts.updateAccount({
        account: { ...account, signature: next.trim() || null },
      })
      // Reflect the saved value back onto our local copy so the
      // next `update_account` call doesn't ship a stale signature
      // when the user keeps typing.
      account.signature = next.trim() || null
      saveStatus = 'saved'
      // Notify the main window so its AccountSettings can refresh
      // the inline editor's `account.signature`.  Best-effort —
      // the user's edit is already persisted backend-side, so a
      // missed event is only a UI staleness, not data loss.
      try {
        await api.emitAppEventToParent(SIGNATURE_UPDATED_EVENT, {
          accountId: account.id,
          html: next,
        })
      } catch (e) {
        console.warn(`${SIGNATURE_UPDATED_EVENT} emit failed`, e)
      }
      setTimeout(() => {
        if (saveStatus === 'saved') saveStatus = ''
      }, 1500)
    } catch (e) {
      console.warn('failed to save signature from popout', e)
      saveStatus = 'error'
    }
  }

  function scheduleSave(next: string) {
    pendingHtml = next
    saveStatus = 'saving'
    if (saveTimer) clearTimeout(saveTimer)
    saveTimer = setTimeout(() => {
      saveTimer = null
      const toPersist = pendingHtml
      pendingHtml = null
      if (toPersist != null) void persist(toPersist)
    }, 400)
  }

  function onSignatureChange(next: string) {
    html = next
    scheduleSave(next)
  }

  /** Flush any pending debounced save synchronously-ish before the
   *  window goes away.  We can't actually block on the IPC round-trip
   *  inside a close handler reliably, but we kick it off so the
   *  backend receives the final value even if the user hits the OS
   *  close button mid-type. */
  async function flushPending(): Promise<void> {
    if (saveTimer) {
      clearTimeout(saveTimer)
      saveTimer = null
    }
    const toPersist = pendingHtml
    pendingHtml = null
    if (toPersist != null) {
      await persist(toPersist)
    }
  }

  async function announceClosing(): Promise<void> {
    if (!account) return
    try {
      await api.emitAppEventToParent(SIGNATURE_POPOUT_CLOSED_EVENT, { accountId: account.id })
    } catch (e) {
      console.warn(`${SIGNATURE_POPOUT_CLOSED_EVENT} emit failed`, e)
    }
  }

  /** One-shot guard so the close path runs exactly once.  The
   *  `onCloseRequested` handler `preventDefault()`s to let us flush
   *  before the window dies; once we kick off `closeWindow()` from
   *  Done OR from the handler itself, this flag short-circuits any
   *  re-entrant fire of the handler. */
  let closing = false
  /** Unsubscribe handle for the close-requested listener.  Hoisted
   *  out of `$effect` so `closeWindow()` can tear the listener down
   *  *before* it calls `close()` — belt-and-braces against a Tauri
   *  release where the synchronous `closing` early-return wouldn't
   *  be enough to break the preventDefault loop. */
  let unlistenClose: (() => void) | null = null

  async function closeWindow() {
    if (closing) return
    closing = true
    await flushPending()
    await announceClosing()
    // Drop our close-requested listener before issuing the
    // programmatic close so there is no possible re-entry.  Without
    // this, Tauri firing the handler one more time on our own
    // `close()` would leave the window in a "close requested but
    // not actually closing" state if anything in the handler chain
    // mistakenly called preventDefault again.
    unlistenClose?.()
    unlistenClose = null
    try {
      await getCurrentWindow().close()
    } catch (e) {
      // Surface any IPC / permission failure so it's debuggable;
      // a silent rejection here would look exactly like "Done does
      // nothing", which is what the bug reporter saw.
      console.error('signature popout close() failed', e)
    }
  }

  $effect(() => {
    let unlistenSystem: (() => void) | null = null

    void (async () => {
      // Theme bootstrap so the popped-out window matches the user's
      // chosen Skeleton theme + light/dark mode.  Mirrors the other
      // standalone windows.
      try {
        const prefs = await api.settings.getAppSettings()
        applyTheme(prefs.theme_name, prefs.theme_mode)
        unlistenSystem = installSystemModeListener(
          prefs.theme_mode,
          prefs.theme_name,
        )
      } catch (e) {
        console.warn('get_app_settings failed in standalone signature editor', e)
      }

      const stashed = takeSignatureEditorPopoutPayload(popoutKey)
      if (!stashed) {
        loadError = m.signature_popout_missing_state()
        return
      }

      // Fetch the full account row so we can round-trip every field
      // through `update_account` — the backend expects the entire
      // Account record, not a partial.
      try {
        const accounts = await api.accounts.getAccounts()
        const match = accounts.find((a) => a.id === stashed.accountId)
        if (!match) {
          loadError = m.signature_popout_account_missing()
          return
        }
        account = match
        // Prefer the live signature from the freshly-fetched account
        // row over the snapshot stashed at launch — covers the (rare)
        // race where another surface updated the signature between
        // the launcher firing and the popout mounting.
        html = (match.signature as string | null) ?? stashed.initialHtml ?? ''
      } catch (e) {
        console.warn('get_accounts failed in standalone signature editor', e)
        loadError = m.signature_popout_load_failed()
        return
      }

      // Intercept the OS-level close so we can flush + announce
      // before the window goes away.  Without this the main window
      // would stay locked because it never received
      // `signature-popout-closed`.
      //
      // The `closing` guard is critical: `closeWindow()` eventually
      // calls `getCurrentWindow().close()` itself, which fires this
      // same handler.  Without the early-out we'd `preventDefault()`
      // forever and the window would never close.
      try {
        unlistenClose = await getCurrentWindow().onCloseRequested(async (event) => {
          if (closing) return
          event.preventDefault()
          await closeWindow()
        })
      } catch (e) {
        console.warn('onCloseRequested registration failed', e)
      }
    })()

    return () => {
      unlistenSystem?.()
      unlistenClose?.()
    }
  })
</script>

<div class="h-screen w-screen flex flex-col bg-surface-50 dark:bg-surface-900">
  {#if loadError}
    <div class="flex-1 flex items-center justify-center text-sm text-error-500 p-6">
      {loadError}
    </div>
  {:else if account}
    <header
      class="flex items-center justify-between gap-3 px-4 py-2 border-b border-surface-200 dark:border-surface-700"
    >
      <div class="min-w-0 flex flex-col">
        <span class="text-sm font-medium truncate"
          >{m.signature_popout_header()}</span
        >
        <span class="text-xs text-surface-500 truncate"
          >{account.display_name || account.email || account.id}</span
        >
      </div>
      <div class="flex items-center gap-3">
        {#if saveStatus === 'saving'}
          <span class="text-xs text-surface-400">{m.signature_status_saving()}</span>
        {:else if saveStatus === 'saved'}
          <span class="text-xs text-success-500">{m.signature_status_saved()}</span>
        {:else if saveStatus === 'error'}
          <span class="text-xs text-error-500">{m.signature_status_error()}</span>
        {/if}
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 text-xs"
          onclick={() => void closeWindow()}
        >{m.signature_popout_done()}</button>
      </div>
    </header>

    <!-- The editor fills the rest of the window.  Tiptap needs a
         bounded-height parent (its scroller is `flex-1 min-h-0`);
         the wrapping flex column gives it exactly that. -->
    <div class="flex-1 min-h-0 flex flex-col">
      <RichTextEditor
        content={html}
        placeholder={m.signature_popout_placeholder()}
        onchange={onSignatureChange}
      />
    </div>
  {:else}
    <div class="flex-1 flex items-center justify-center text-sm text-surface-500">
      {m.signature_popout_loading()}
    </div>
  {/if}
</div>
