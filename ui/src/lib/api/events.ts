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

import { listen, emit, type Event, type UnlistenFn } from '@tauri-apps/api/event'

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
  'reminder-show-event': { eventId: string }
  'signature-updated-from-popout': { accountId: string; html: string }
  'signature-popout-closed': { accountId: string }
}

export type AppEventName = keyof AppEventPayloads

export function onAppEvent<K extends AppEventName>(
  name: K,
  handler: (event: Event<AppEventPayloads[K]>) => void,
): Promise<UnlistenFn> {
  return listen(name, handler)
}

export function emitAppEvent<K extends AppEventName>(
  name: K,
  payload: AppEventPayloads[K],
): Promise<void> {
  return emit(name, payload)
}
