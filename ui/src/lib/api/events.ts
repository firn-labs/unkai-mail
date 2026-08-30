/**
 * Typed event channel between backend, main window, and popouts (#473).
 *
 * `AppEventPayloads` is the single registry of every event name in the
 * app — the backend-emitted push channels (new mail, unread counts,
 * reminders, …) and the popout-to-main handoff events the standalone
 * windows emit. Subscribing or emitting an unregistered name is a
 * compile error, which is the point: event names used to be bare
 * strings scattered across components.
 *
 * Handlers receive the raw Tauri `Event<T>` (payload under
 * `event.payload`) so existing handler bodies work unchanged.
 */

import { emit, emitTo, type Event, type UnlistenFn } from '@tauri-apps/api/event'
import { getCurrentWebviewWindow, WebviewWindow } from '@tauri-apps/api/webviewWindow'
import { parentWindowLabel } from '../windowContext'

/* Popout event names that predate this module keep their historical
 * string values — the Rust side and any already-open popout window
 * match on them literally. */
export const SIGNATURE_UPDATED_EVENT = 'signature-updated-from-popout'
export const SIGNATURE_POPOUT_CLOSED_EVENT = 'signature-popout-closed'

export interface AppEventPayloads {
  /* ── backend → UI push channels ─────────────────────────────── */
  'new-mail': any
  'mail-flags-updated': any
  'unread-count-updated': any
  'unread-count-by-account-updated': any
  'outbox-updated': any
  'event-reminder': any
  'message-reminder': any
  'calendars-updated': any
  'custom-themes-changed': any
  /* #534 — the machine-global profile registry changed (create /
   * rename / re-icon / delete / startup-mode). Broadcast to every
   * window, not just the emitting profile's; payload is empty —
   * listeners re-read via api.profiles. */
  'profiles-changed': null
  'notification-clicked': any
  'open-compose': any
  /* #229 — update-bundle download progress, targeted at the window
   * that invoked `download_app_update`.  `total` is null when the
   * server sent no Content-Length. */
  'update-download-progress': { downloaded: number; total: number | null }
  /* ── popout window → main window handoffs ───────────────────── */
  'compose-from-mail': any
  'respond-with-meeting-from-mail': any
  'edit-draft-from-mail': any
  'mailto-from-mail': any
  'event-editor-saved-from-popout': any
  /* OS-level mailto handoff (#294) — the Rust side forwards each
   * `mailto:` URL the OS hands us through this channel. Historical
   * name, matched literally by the backend emit. */
  'unkai://mailto': string
  /* OS-level file-open handoff (#536) — an `.eml` / `.ics` path a
   * second launch handed the running instance, targeted at the
   * most recently focused profile window by `external_open.rs`.
   * Cold-start paths ride the `take_pending_files_to_open` drain
   * instead. */
  'unkai://open-file': string
  'reminder-show-event': { eventId: string }
  'signature-updated-from-popout': { accountId: string; html: string }
  'signature-popout-closed': { accountId: string }
}

export type AppEventName = keyof AppEventPayloads

/**
 * Subscribe to an app event — scoped to THIS window (#535).
 *
 * The listener is registered against the current webview window,
 * not globally: it receives broadcasts (`emitAppEvent`,
 * `profiles-changed`) plus emits targeted at this window's label,
 * and nothing else.  That scoping is what makes the backend's
 * per-profile `emit_to` targeting effective — a global `listen()`
 * would receive every profile's push events in every window.
 */
export function onAppEvent<K extends AppEventName>(
  name: K,
  handler: (event: Event<AppEventPayloads[K]>) => void,
): Promise<UnlistenFn> {
  return getCurrentWebviewWindow().listen(name, handler)
}

/** Broadcast to every window — reach for the targeted variants
 *  below first; with several profile windows open a broadcast
 *  fires every shell's handler at once. */
export function emitAppEvent<K extends AppEventName>(
  name: K,
  payload: AppEventPayloads[K],
): Promise<void> {
  return emit(name, payload)
}

/**
 * Emit a popout→main handoff to the window that spawned this
 * popout (#535) — the `parent` URL param the shared popout helper
 * stamps.  Falls back to a broadcast when the param is missing OR
 * the parent window has since been destroyed: an `emitTo` against
 * a dead label resolves successfully and the handoff would be
 * silently lost (a Reply click doing nothing), while the
 * broadcast reaches whichever shells are alive — the pre-#535
 * behaviour, cross-profile-noisy but never silent.
 */
export async function emitAppEventToParent<K extends AppEventName>(
  name: K,
  payload: AppEventPayloads[K],
): Promise<void> {
  if (parentWindowLabel && (await WebviewWindow.getByLabel(parentWindowLabel))) {
    return emitTo(parentWindowLabel, name, payload)
  }
  return emit(name, payload)
}
