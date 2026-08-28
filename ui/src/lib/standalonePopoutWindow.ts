/**
 * THE shared popout-window helper (#535).
 *
 * Every standalone window (compose #110, mail reader #104, .eml
 * viewer #254, event editor #304, signature editor #314, reminder
 * popup #203, NC attachment viewer #162) used to hand-roll the
 * same pattern: stash a payload in `localStorage` under a UUID
 * key → `new WebviewWindow(label, { url: 'index.html?view=…' })`
 * → the standalone component reads-and-deletes on mount.  This
 * module owns that pattern once, and adds the two #535 duties the
 * copies would each have needed:
 *
 *   1. **Profile registration** — before the window is created,
 *      `register_popout_window` maps its label to the calling
 *      window's profile in the backend registry, so the popout's
 *      very first IPC command already resolves to the right
 *      profile (a work-profile popout must talk to the work
 *      profile's cache).  Unregistered labels fall back to the
 *      startup profile.
 *   2. **Parent threading** — the URL carries `parent=<label>`:
 *      the window the popout's handoff events should target (see
 *      `api/events.emitAppEventToParent`).  A popout spawned from
 *      another popout forwards its own parent, so handoffs always
 *      land in the primary window that started the chain.
 *
 * Payload transport stays localStorage: Tauri webview windows
 * sharing an origin share localStorage, so the handoff is one-shot
 * and synchronous — an event-based handshake would need a mount
 * race dance.  NOTE: localStorage caps at ~5–10 MB per origin;
 * very large compose attachments may overflow (accepted for v1).
 */

import { getCurrentWebviewWindow, WebviewWindow } from '@tauri-apps/api/webviewWindow'
import * as api from './api'
import { parentWindowLabel } from './windowContext'

/** The `WebviewWindow` options the popouts actually use. */
export interface PopoutWindowOptions {
  title: string
  width: number
  height: number
  minWidth?: number
  minHeight?: number
  resizable?: boolean
  decorations?: boolean
  alwaysOnTop?: boolean
  skipTaskbar?: boolean
  focus?: boolean
  x?: number
  y?: number
}

export interface PopoutSpec {
  /** `main.ts` route (`?view=…`); doubles as the label prefix, so
   *  it must stay within the window patterns declared in
   *  `src-tauri/capabilities/default.json`. */
  view: 'mail' | 'mailfile' | 'compose' | 'reminder' | 'event-editor' | 'signature-editor'
  /** Stash `payload` under `payloadPrefix + key` and put `key=` in
   *  the URL.  Omit both for popouts that carry their state in the
   *  URL (`params`) instead. */
  payloadPrefix?: string
  payload?: unknown
  /** Extra URL query params (mail/mailfile state). */
  params?: Record<string, string>
  window: PopoutWindowOptions
}

/**
 * Open a standalone popout window.  Resolves once the window is
 * created (not mounted).
 */
export async function openPopout(spec: PopoutSpec): Promise<WebviewWindow> {
  const key = crypto.randomUUID().replaceAll('-', '')
  const label = `${spec.view}-${key}`

  const params = new URLSearchParams({ view: spec.view, ...(spec.params ?? {}) })
  if (spec.payloadPrefix !== undefined) {
    // Stash before opening so the new window can read it as soon
    // as its JS mounts — no race, both windows share the
    // localStorage origin.
    localStorage.setItem(spec.payloadPrefix + key, JSON.stringify(spec.payload))
    params.set('key', key)
  }
  // Handoff events target the primary window that started the
  // chain: a primary window hands its own label down, a popout
  // forwards the parent it was given.
  params.set('parent', parentWindowLabel ?? getCurrentWebviewWindow().label)

  // Map the label to this window's profile BEFORE creation (#535)
  // — awaited, so the popout's first command can't race the
  // registration and fall back to the startup profile.
  try {
    await api.system.registerPopoutWindow({ label })
  } catch (e) {
    console.warn('popout profile registration failed (startup-profile fallback):', e)
  }

  return new WebviewWindow(label, {
    url: `index.html?${params.toString()}`,
    ...spec.window,
  })
}

/**
 * Open a popout onto an external URL (the NC attachment viewer) —
 * same window plumbing, no app bundle, no payload.  The label
 * still gets a fresh UUID under the given prefix; no profile
 * registration, because the external page never issues IPC.
 */
export function openExternalPopout(
  labelPrefix: string,
  url: string,
  options: PopoutWindowOptions,
): WebviewWindow {
  const label = `${labelPrefix}-${crypto.randomUUID().replaceAll('-', '')}`
  return new WebviewWindow(label, { url, ...options })
}

/**
 * Read + clear a popout payload.  Called by the standalone
 * component exactly once at mount; returns `null` when the key is
 * missing or the JSON is malformed — each caller has its own
 * fallback (blank compose, load error, close).
 */
export function takePopoutPayload<T>(prefix: string, key: string): T | null {
  const fullKey = prefix + key
  const raw = localStorage.getItem(fullKey)
  if (!raw) return null
  localStorage.removeItem(fullKey)
  try {
    return JSON.parse(raw) as T
  } catch (e) {
    console.warn(`takePopoutPayload(${prefix}): malformed JSON`, e)
    return null
  }
}
