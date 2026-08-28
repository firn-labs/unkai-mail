/**
 * Spawn a standalone Tauri webview window for Compose (#110).
 *
 * The new window is the same Vite bundle, but `main.ts` checks
 * `view=compose` and mounts `StandaloneCompose.svelte` instead of
 * `App.svelte`.  All the window plumbing — payload stash, label,
 * profile registration, parent threading — lives in the shared
 * `standalonePopoutWindow.ts` helper (#535).
 */

import type { ComposeInitial } from './Compose.svelte'
import { openPopout, takePopoutPayload } from './standalonePopoutWindow'

/** What we hand to the standalone window. Same shape as
 *  ComposeInitial plus the From-account selection (which lives on
 *  Compose itself, separate from the modal-open accountId prop). */
export interface ComposePopoutPayload {
  accountId: string
  initial: ComposeInitial
}

const STORAGE_KEY_PREFIX = 'unkai-compose-popout-'

export async function openComposeInStandaloneWindow(
  payload: ComposePopoutPayload,
): Promise<void> {
  // `focus: true` makes the new window come up in the foreground —
  // matters most for #304, where the trigger is a button click in
  // a popped-out mail window and the user is staring at that
  // surface, not the main window.  Tauri's WindowBuilder default
  // is platform-dependent, so we set it explicitly.
  await openPopout({
    view: 'compose',
    payloadPrefix: STORAGE_KEY_PREFIX,
    payload,
    window: {
      title: payload.initial.subject || 'Compose — Unkai Mail',
      width: 800,
      height: 700,
      minWidth: 500,
      minHeight: 400,
      focus: true,
    },
  })
}

/** Read + clear the popout payload for the given key.  Called by
 *  `StandaloneCompose.svelte` exactly once at mount.  Returns null
 *  when the key is missing or the JSON is malformed — the caller
 *  falls back to a blank compose so the window isn't useless. */
export function takeComposePopoutPayload(
  key: string,
): ComposePopoutPayload | null {
  return takePopoutPayload<ComposePopoutPayload>(STORAGE_KEY_PREFIX, key)
}
