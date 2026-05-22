/**
 * Spawn a standalone Tauri webview window for Compose (#110).
 *
 * Mirrors the standalone-mail flow: the new window is the same Vite
 * bundle, but `main.ts` checks `view=compose` and mounts
 * `StandaloneCompose.svelte` instead of `App.svelte`.
 *
 * The Compose state (To / Cc / Bcc / subject / body / attachments /
 * From account / draft source / reply context) gets stashed in
 * `localStorage` under a UUID key and the URL carries just the key.
 * Tauri webview windows that share an origin share localStorage, so
 * the popped-out window reads the payload and removes the entry on
 * mount.  We could pass via Tauri events instead but that needs a
 * mount-handshake; localStorage is one-shot and synchronous.
 *
 * NOTE: localStorage caps at ~5–10 MB per origin.  Very large
 * attachments (a 20 MB PDF after base64 encoding crosses that line)
 * may overflow.  Acceptable for v1 — Compose itself already capped
 * attachment sizes and the existing Tauri send path is the limiting
 * factor anyway.
 */

import type { ComposeInitial } from './Compose.svelte'

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
  const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow')
  const key = crypto.randomUUID().replaceAll('-', '')
  // Stash before opening so the new window can read it as soon as
  // its JS mounts — there's no race because both windows share the
  // same localStorage origin.
  localStorage.setItem(STORAGE_KEY_PREFIX + key, JSON.stringify(payload))
  // `focus: true` makes the new window come up in the foreground —
  // matters most for #304, where the trigger is a button click in
  // a popped-out mail window and the user is staring at that
  // surface, not the main window.  Tauri's WindowBuilder default
  // is platform-dependent, so we set it explicitly.
  new WebviewWindow(`compose-${key}`, {
    url: `index.html?view=compose&key=${key}`,
    title: payload.initial.subject || 'Compose — Unkai Mail',
    width: 800,
    height: 700,
    minWidth: 500,
    minHeight: 400,
    focus: true,
  })
}

/** Read + clear the popout payload for the given key.  Called by
 *  `StandaloneCompose.svelte` exactly once at mount.  Returns null
 *  when the key is missing or the JSON is malformed — the caller
 *  falls back to a blank compose so the window isn't useless. */
export function takeComposePopoutPayload(
  key: string,
): ComposePopoutPayload | null {
  const fullKey = STORAGE_KEY_PREFIX + key
  const raw = localStorage.getItem(fullKey)
  if (!raw) return null
  localStorage.removeItem(fullKey)
  try {
    return JSON.parse(raw) as ComposePopoutPayload
  } catch (e) {
    console.warn('takeComposePopoutPayload: malformed JSON', e)
    return null
  }
}
