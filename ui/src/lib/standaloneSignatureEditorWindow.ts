/**
 * Spawn a standalone Tauri webview window for the signature editor
 * (#314).
 *
 * Mirrors `standaloneComposeWindow.ts`: the new window is the same
 * Vite bundle, but `main.ts` checks `view=signature-editor` and
 * mounts `StandaloneSignatureEditor.svelte` instead of `App.svelte`.
 *
 * The launch payload (which account is being edited, plus the
 * starting HTML) is stashed in `localStorage` under a UUID key and
 * the URL only carries the key.  Tauri webview windows sharing the
 * same origin share localStorage, so the popped-out window reads the
 * payload synchronously on mount.
 *
 * Save flow is auto-save, same as the inline editor in
 * `AccountSettings.svelte` — every keystroke (debounced) writes
 * through `update_account` from inside the popout and emits
 * `signature-updated-from-popout` so the settings panel in the main
 * window can refresh its in-memory `account.signature`.  When the
 * popout closes it emits `signature-popout-closed`, which is the
 * signal the main window uses to re-enable its inline editor.
 */

import { openPopout, takePopoutPayload } from './standalonePopoutWindow'

/** What we hand to the standalone window. */
export interface SignatureEditorPopoutPayload {
  /** Account row id this signature belongs to.  The popout looks
   *  the full account up via `get_accounts` rather than receiving
   *  the entire row — that keeps the localStorage entry tiny and
   *  avoids serialising fields the user isn't allowed to edit from
   *  this window (host / port / passwords stay backend-side). */
  accountId: string
  /** Friendly account label for the window title.  Falls back to
   *  the email address if the account has no display name set. */
  accountLabel: string
  /** Initial signature HTML.  Empty string when the account has no
   *  signature yet (the inline editor passes `account.signature ?? ''`). */
  initialHtml: string
}

const STORAGE_KEY_PREFIX = 'unkai-signature-popout-'

export async function openSignatureEditorInStandaloneWindow(
  payload: SignatureEditorPopoutPayload,
): Promise<void> {
  await openPopout({
    view: 'signature-editor',
    payloadPrefix: STORAGE_KEY_PREFIX,
    payload,
    window: {
      title: `Signature — ${payload.accountLabel}`,
      width: 900,
      height: 700,
      minWidth: 500,
      minHeight: 400,
      focus: true,
    },
  })
}

/** Read + clear the popout payload for the given key.  Called by
 *  `StandaloneSignatureEditor.svelte` exactly once at mount.
 *  Returns null when the key is missing or the JSON is malformed —
 *  the caller surfaces an inline error and lets the user close the
 *  window manually. */
export function takeSignatureEditorPopoutPayload(
  key: string,
): SignatureEditorPopoutPayload | null {
  return takePopoutPayload<SignatureEditorPopoutPayload>(STORAGE_KEY_PREFIX, key)
}

/* The popout's event names live in the typed registry in `api/events`
 * (#473); re-exported here so the window helper stays the popout's
 * single import point. */
export { SIGNATURE_UPDATED_EVENT, SIGNATURE_POPOUT_CLOSED_EVENT } from './api/events'
