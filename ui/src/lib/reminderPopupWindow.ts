/**
 * Spawn a standalone Tauri webview window for a calendar
 * reminder (#203 follow-up).
 *
 * Mirrors the standalone-compose flow (`standaloneComposeWindow.ts`):
 * the new window is the same Vite bundle but `main.ts` checks
 * `view=reminder` and mounts `StandaloneReminder.svelte` instead of
 * `App.svelte`.  Payload is stashed in `localStorage` under a UUID
 * key and the URL carries just the key — Tauri webview windows that
 * share an origin share `localStorage`, so the popped-out window
 * reads the payload synchronously on mount.
 *
 * Why a separate window instead of an in-app overlay:
 *   * The popup needs to surface even when the main Unkai window
 *     is hidden / minimised to the tray, which an in-app overlay
 *     can't do (it lives inside the main window).
 *   * Picks up `alwaysOnTop` so it sits above the user's current
 *     foreground app — the standard behaviour for desktop
 *     calendar-reminder popups.
 */

import { openPopout, takePopoutPayload } from './standalonePopoutWindow'

/** Same shape as the backend `EventReminderPayload`. */
export interface EventReminderPayload {
  eventId: string
  uid: string
  summary: string
  start: string
  end: string
  location: string | null
  attendees: string[]
  meetingUrl: string | null
  minutesBefore: number
}

const STORAGE_KEY_PREFIX = 'unkai-reminder-popup-'

/** Window dimensions.  Sized to comfortably fit title + a 2-3 line
 *  summary + the action row, without being so big that it eats the
 *  user's screen real-estate. */
const WINDOW_WIDTH = 380
const WINDOW_HEIGHT = 320
/** Margin from the screen edge.  Keeps the window from touching
 *  the taskbar / dock.  `availWidth` / `availHeight` already
 *  excludes the taskbar on Windows + the menu bar on macOS, so
 *  the margin is purely cosmetic. */
const EDGE_MARGIN = 12

export async function openReminderInStandaloneWindow(
  payload: EventReminderPayload,
): Promise<void> {
  // Bottom-right of the user's primary screen, above the taskbar.
  // `screen.availWidth` / `availHeight` is the work area on every
  // major OS (Windows excludes taskbar, macOS excludes the menu
  // bar + dock, GNOME/KDE exclude top + bottom panels).
  const x = Math.max(0, window.screen.availWidth - WINDOW_WIDTH - EDGE_MARGIN)
  const y = Math.max(0, window.screen.availHeight - WINDOW_HEIGHT - EDGE_MARGIN)

  await openPopout({
    view: 'reminder',
    payloadPrefix: STORAGE_KEY_PREFIX,
    payload,
    window: {
      title: payload.summary || 'Unkai Reminder',
      width: WINDOW_WIDTH,
      height: WINDOW_HEIGHT,
      minWidth: WINDOW_WIDTH,
      minHeight: WINDOW_HEIGHT,
      resizable: false,
      decorations: false,
      alwaysOnTop: true,
      skipTaskbar: true,
      focus: true,
      // `x` and `y` are interpreted as logical pixels, so HiDPI
      // displays place the window where we expect.
      x,
      y,
    },
  })
}

/** Read + clear the popout payload for the given key.  Called by
 *  `StandaloneReminder.svelte` exactly once at mount.  Returns null
 *  when the key is missing or the JSON is malformed — the caller
 *  closes the window in that case so we don't surface a useless
 *  reminder. */
export function takeReminderPopoutPayload(
  key: string,
): EventReminderPayload | null {
  return takePopoutPayload<EventReminderPayload>(STORAGE_KEY_PREFIX, key)
}
