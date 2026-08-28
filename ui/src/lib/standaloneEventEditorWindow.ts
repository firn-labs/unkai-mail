/**
 * Spawn a standalone Tauri webview window for EventEditor (#304).
 *
 * Mirrors the standalone-compose flow: the new window loads the same
 * Vite bundle with `?view=event-editor&key=<id>` in the URL; `main.ts`
 * routes that into `StandaloneEventEditor.svelte` (instead of the main
 * `App.svelte`).
 *
 * The editor's seed (calendars, draft details, reply context) is
 * stashed in `localStorage` under a UUID key and the URL carries just
 * the key.  Tauri webview windows that share an origin share
 * localStorage, so the popped-out window reads the payload and removes
 * the entry on mount.
 *
 * Dates are JSON-unfriendly, so we ship them as ISO strings on the
 * wire and the receiving side re-hydrates into `Date` objects before
 * handing them to EventEditor.
 *
 * The `replyTo` field is opaque here — we round-trip it untouched
 * back to the main window via the `event-editor-saved-from-popout`
 * Tauri event so App.svelte can re-narrow it to its `ReplyableMail`
 * shape and open the final Compose pre-filled as a reply.  Keeping
 * the type unknown avoids dragging the `ReplyableMail` shape across
 * a window boundary.
 */

import { openPopout, takePopoutPayload } from './standalonePopoutWindow'

/** Snapshot of `CalendarSummary` — mirrors the in-App shape, kept
 *  local because the source defines it inside a Svelte `<script>`
 *  block and can't be imported across modules cleanly. */
export interface CalendarSummaryPopout {
  id: string
  nextcloud_account_id: string
  display_name: string
  color: string | null
  last_synced_at: string | null
  hidden?: boolean
  muted?: boolean
  read_only?: boolean
}

/** create-mode seed for EventEditor, with Date fields flattened to
 *  ISO strings for JSON transport.  Mirrors the in-App `meetingDraft.
 *  draft` shape (which uses real Dates); the popout window converts
 *  back on mount. */
export interface EventEditorDraftPopout {
  calendarId: string
  /** ISO 8601 — the popout window parses back into a `Date`. */
  start: string
  /** ISO 8601 — the popout window parses back into a `Date`. */
  end: string
  allDay?: boolean
  summary?: string
  description?: string
  location?: string
  url?: string
  attendees?: string[]
  requiredAttendees?: string[]
  optionalAttendees?: string[]
  chairAttendees?: string[]
  createTalkRoom?: boolean
}

/** What we hand to the standalone window.  `replyTo` is opaque — the
 *  popout doesn't introspect it; it gets forwarded back to the main
 *  window on save so App.svelte can build the final Compose. */
export interface EventEditorPopoutPayload {
  calendars: CalendarSummaryPopout[]
  draft: EventEditorDraftPopout
  /** Original mail the user clicked "Respond with meeting" on.
   *  `unknown` because the popout window never reads it — it just
   *  ships it back through the saved-from-popout event so App.svelte
   *  can narrow it back to `ReplyableMail`. */
  replyTo?: unknown
}

const STORAGE_KEY_PREFIX = 'unkai-event-editor-popout-'

export async function openEventEditorInStandaloneWindow(
  payload: EventEditorPopoutPayload,
): Promise<void> {
  // `focus: true` brings the new window to the foreground.  The
  // trigger is a click in a popped-out mail window, so the user is
  // looking at that surface — they should see the editor right
  // away rather than have to hunt for it.
  await openPopout({
    view: 'event-editor',
    payloadPrefix: STORAGE_KEY_PREFIX,
    payload,
    window: {
      title: payload.draft.summary || 'New event — Unkai Mail',
      width: 700,
      height: 760,
      minWidth: 500,
      minHeight: 500,
      focus: true,
    },
  })
}

/** Read + clear the popout payload for the given key.  Called by
 *  `StandaloneEventEditor.svelte` exactly once at mount.  Returns
 *  null when the key is missing or the JSON is malformed — the
 *  caller surfaces a load error so the window isn't a silent
 *  empty shell. */
export function takeEventEditorPopoutPayload(
  key: string,
): EventEditorPopoutPayload | null {
  return takePopoutPayload<EventEditorPopoutPayload>(STORAGE_KEY_PREFIX, key)
}
