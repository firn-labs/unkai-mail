<script lang="ts">
  /**
   * CalendarView — week grid layout.
   *
   * Layout: seven day columns with a 24h time axis, events positioned
   * absolutely inside each column, plus an all-day strip above the
   * grid. Navigation is week-at-a-time with a "Today" shortcut.
   *
   * # Fetch strategy
   *
   * On mount we pull a broad window (±6 months) so week navigation
   * feels instant. When the user scrolls past the edge of the loaded
   * window (within `EXTEND_THRESHOLD_DAYS` of either boundary), we
   * extend that direction by `EXTEND_CHUNK_DAYS` and re-query the
   * backend — recurrence expansion is per-query server-side, so this
   * falls out cleanly without any client-side expansion.
   *
   * # Data flow
   *
   * - `init()` → accounts → `reloadFromCache(window)` → background sync.
   * - Week nav → adjust `currentWeekStart` → derived `weekEvents`
   *   filters the in-memory `events` array.
   * - Out-of-window nav → `ensureWindowCovers()` re-fetches with a
   *   widened window.
   *
   * # What's deliberately out of scope
   *
   * Event editing, month/day toggle, click-to-create, drag-to-resize
   * — each of those is its own issue. This component stays a
   * read-only week renderer.
   */

  import * as api from './api'
  import type { UnlistenFn } from '@tauri-apps/api/event'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'
  import { m } from '../paraglide/messages'
  import EventEditor, { type SavedEvent } from './EventEditor.svelte'
  import Select from './Select.svelte'
  import { resizableSidebar } from './resizableSidebar'

  interface Props {
    /** When set (by the in-app reminder card's "Show event"
     *  action, #203), CalendarView opens this event's editor
     *  as soon as the matching event is loaded into `events`.
     *  CalendarView fires `oneventfocused` immediately after
     *  to clear the parent's state so the same id doesn't
     *  re-trigger on a later mount. */
    focusEventId?: string | null
    oneventfocused?: () => void
  }
  const {
    focusEventId = null,
    oneventfocused,
  }: Props = $props()

  // ── Types (mirror the Rust models) ──────────────────────────
  interface NextcloudAccount {
    id: string
    server_url: string
    username: string
    display_name?: string | null
  }
  interface CalendarSummary {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    /** Layer 1 (Settings). `true` removes the calendar from the sidebar
     *  entirely. Toggled from NextcloudSettings' per-calendar checkboxes. */
    hidden?: boolean
    /** Layer 2 (sidebar swatch). `true` keeps the calendar in the sidebar
     *  but stops its events from painting on the agenda grid. Toggled via
     *  the coloured swatch button in the CalendarView sidebar. */
    muted?: boolean
    /** CalDAV-derived read-only flag (#236).  Drives the
     *  EventEditor's picker filter + Delete-button hide. */
    read_only?: boolean
  }
  interface EventAttendee {
    email: string
    common_name?: string | null
    status?: string | null
  }
  interface EventReminder {
    trigger_minutes_before: number
    action?: string | null
  }
  interface CalendarEvent {
    id: string
    summary: string
    description: string | null
    start: string // RFC3339
    end: string
    location: string | null
    rrule: string | null
    rdate: string[]
    exdate: string[]
    recurrence_id: string | null
    url?: string | null
    transparency?: string | null
    attendees?: EventAttendee[]
    reminders?: EventReminder[]
  }

  // ── Layout constants ────────────────────────────────────────
  // 48px per hour gives each 30-minute slot a readable 24px band.
  // Pixel-per-minute math elsewhere derives from this single number
  // so tweaking density is a one-line change.
  const HOUR_HEIGHT_PX = 48
  const PX_PER_MINUTE = HOUR_HEIGHT_PX / 60
  // ISO-day number for Monday. Users expect a Mon-start week for a
  // European-flavoured app; the backend is already storing in UTC so
  // switching to Sunday later is purely a UI change.
  const WEEK_STARTS_ON = 1
  // Top/bottom strips for all-day events — each row is a colored pill.
  // If more than `ALL_DAY_VISIBLE_ROWS` events fall on the same day,
  // the surplus collapses into a "+N more" affordance rather than
  // pushing the time grid off-screen.
  const ALL_DAY_ROW_HEIGHT_PX = 24
  const ALL_DAY_VISIBLE_ROWS = 3

  // ── Fetch-window constants ──────────────────────────────────
  // Initial load covers ±6 months — almost every week-nav stays
  // inside this span without hitting the backend. Picked empirically:
  // Nextcloud parsing overhead for a year of events is fine (<200ms
  // on the test account) but memory in Svelte gets chatty past that.
  const INITIAL_PAST_DAYS = 180
  const INITIAL_FUTURE_DAYS = 180
  // When the user navigates close to the edge of the loaded window
  // we extend it. The threshold is intentionally larger than a single
  // week so the extension happens one week *before* they'd see an
  // empty week, not as they're looking at it.
  const EXTEND_THRESHOLD_DAYS = 21
  const EXTEND_CHUNK_DAYS = 120

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<NextcloudAccount[]>([])
  let calendars = $state<CalendarSummary[]>([])
  let events = $state<CalendarEvent[]>([])

  let loading = $state(true)
  let syncing = $state(false)
  let error = $state('')
  let syncErrors = $state<string[]>([])

  // Current navigation focus: the Monday of the visible week.
  let currentWeekStart = $state<Date>(startOfWeek(new Date()))
  // Live "now" — drives the today-column current-time line and
  // the gray "past" overlay on today.  Ticked every 30s; cheap
  // because Svelte 5's reactivity only re-renders the dependent
  // overlay/line, not the whole grid.
  let now = $state(new Date())
  $effect(() => {
    const id = setInterval(() => {
      now = new Date()
    }, 30_000)
    return () => clearInterval(id)
  })
  const nowMinutes = $derived(now.getHours() * 60 + now.getMinutes())
  // What range of data we currently have fetched. Tracked so week
  // nav can tell whether it's crossing into uncached territory and
  // trigger an extension.
  let loadedRangeStart = $state<Date>(new Date(0))
  let loadedRangeEnd = $state<Date>(new Date(0))

  // Layer 2: muted calendars' events are hidden from the grid. Derived
  // as a Set for O(1) per-event lookup. Newly-discovered calendars
  // default to unmuted automatically (Set of exceptions, not inclusions).
  const mutedCalendarIds = $derived(
    new Set(calendars.filter((c) => c.muted).map((c) => c.id)),
  )
  /** Calendars shown in the sidebar: everything not hidden by Settings
   *  (Layer 1). The muted flag (Layer 2) only affects event rendering. */
  const sidebarCalendars = $derived(calendars.filter((c) => !c.hidden))

  /** Pretty hostname extracted from a Nextcloud `server_url`,
   *  e.g. "https://cloud.example.com/" → "cloud.example.com".
   *  Falls back to the raw URL on parse failure so the user
   *  always sees *something* identifying the server. */
  function ncHostname(serverUrl: string): string {
    try {
      return new URL(serverUrl).host
    } catch {
      return serverUrl
    }
  }
  /** Sidebar calendars grouped by their owning NC account, in
   *  the order the accounts were configured.  Each group gets a
   *  divider + hostname header so users with multiple servers
   *  can tell at a glance which calendars belong where. */
  const sidebarGroups = $derived.by(() => {
    const byNc = new Map<string, CalendarSummary[]>()
    for (const c of sidebarCalendars) {
      const list = byNc.get(c.nextcloud_account_id) ?? []
      list.push(c)
      byNc.set(c.nextcloud_account_id, list)
    }
    const out: { ncId: string; label: string; calendars: CalendarSummary[] }[] = []
    // Walk `accounts` first so the visual order matches the
    // Settings list; fall back to insertion order for any
    // calendar whose account isn't in `accounts` yet (still
    // loading).
    const seen = new Set<string>()
    for (const a of accounts) {
      const list = byNc.get(a.id)
      if (!list) continue
      out.push({ ncId: a.id, label: ncHostname(a.server_url), calendars: list })
      seen.add(a.id)
    }
    for (const [ncId, list] of byNc) {
      if (seen.has(ncId)) continue
      out.push({ ncId, label: ncId, calendars: list })
    }
    return out
  })
  /** Calendars whose events paint on the grid: sidebar calendars that
   *  are also not muted (Layer 2). */
  const visibleCalendars = $derived(sidebarCalendars.filter((c) => !c.muted))

  // ── Calendar management (Issue #82) ─────────────────────────
  // Right-click a calendar row → Rename / Change color / Delete.
  // Top-level `+` button → new calendar modal with name + color.
  // Each operation owns its own `$state` slot, mirroring the
  // folder-management pattern in Sidebar.svelte so the same UX
  // shape carries across the app.

  /** Right-click menu state — anchors a popup at `{x, y}` for the
   *  selected calendar. */
  let calendarContextMenu = $state<{
    calendar: CalendarSummary
    x: number
    y: number
  } | null>(null)

  /** Inline rename — `calendar_id` while the row's text is swapped
   *  for an `<input>`. Matches the folder-rename pattern. */
  let renamingCalendarId = $state<string | null>(null)
  let calendarRenameValue = $state('')

  /** Active color-picker modal, if any. `color` is the current
   *  working swatch — `commit` writes it via `update_calendar`. */
  let colorPicker = $state<{ calendar: CalendarSummary; color: string } | null>(null)

  /** "New calendar" modal state. Only open when non-null; `ncId`
   *  is the target Nextcloud account (auto-picked when there's
   *  only one, user-picked otherwise via a small account select). */
  let newCalendarForm = $state<{
    ncId: string
    displayName: string
    color: string
  } | null>(null)

  /** Destructive-op confirm. */
  let deleteCalendarConfirm = $state<CalendarSummary | null>(null)

  /** Shared busy / error slots — gate double-clicks during an
   *  in-flight CalDAV request, surface errors inline. */
  let calendarOpBusy = $state(false)
  let calendarOpError = $state('')

  const COLOR_PRESETS = [
    '#2bb0ed', '#4caf50', '#8e44ad', '#e67e22', '#e74c3c', '#f39c12',
    '#16a085', '#34495e', '#c0392b', '#d35400', '#27ae60', '#2980b9',
  ]

  function openCalendarContextMenu(e: MouseEvent, calendar: CalendarSummary) {
    e.preventDefault()
    // Any existing edit loses focus when the menu opens on a
    // different row — cleaner than juggling priorities.
    renamingCalendarId = null
    calendarContextMenu = { calendar, x: e.clientX, y: e.clientY }
  }

  function closeCalendarContextMenu() {
    calendarContextMenu = null
    calendarOpError = ''
  }

  $effect(() => {
    if (!calendarContextMenu) return
    const onDocMouseDown = () => closeCalendarContextMenu()
    const onDocKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeCalendarContextMenu()
    }
    document.addEventListener('mousedown', onDocMouseDown)
    document.addEventListener('keydown', onDocKey)
    return () => {
      document.removeEventListener('mousedown', onDocMouseDown)
      document.removeEventListener('keydown', onDocKey)
    }
  })

  // Esc dismisses the "New calendar" modal — same UX shortcut as
  // the EventEditor.  Skip if the account-picker dropdown is open
  // (its own listbox handles Esc to close just the dropdown).
  $effect(() => {
    if (!newCalendarForm) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      if (document.querySelector('[role="listbox"]')) return
      e.preventDefault()
      newCalendarForm = null
      calendarOpError = ''
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  })

  /** Refresh the calendar list + events from the cache after a
   *  mutation. Uses the current visible range so the agenda picks
   *  up any CASCADE deletes from the removed calendar. */
  async function refreshCalendars() {
    if (loadedRangeStart.getTime() > 0) {
      await reloadFromCache(loadedRangeStart, loadedRangeEnd)
    }
  }

  async function commitCalendarRename() {
    if (!renamingCalendarId || calendarOpBusy) return
    const id = renamingCalendarId
    const current = calendars.find((c) => c.id === id)
    const newName = calendarRenameValue.trim()
    if (!current || !newName || newName === current.display_name) {
      renamingCalendarId = null
      calendarRenameValue = ''
      return
    }
    calendarOpBusy = true
    try {
      await api.calendar.updateNextcloudCalendar({
        calendarId: id,
        displayName: newName,
        color: null,
      })
      renamingCalendarId = null
      calendarRenameValue = ''
      await refreshCalendars()
    } catch (e) {
      calendarOpError = formatError(e) || 'Failed to rename calendar'
    } finally {
      calendarOpBusy = false
    }
  }

  function cancelCalendarRename() {
    renamingCalendarId = null
    calendarRenameValue = ''
  }

  async function commitColorChange() {
    if (!colorPicker || calendarOpBusy) return
    const { calendar, color } = colorPicker
    calendarOpBusy = true
    try {
      await api.calendar.updateNextcloudCalendar({
        calendarId: calendar.id,
        displayName: null,
        color,
      })
      colorPicker = null
      await refreshCalendars()
    } catch (e) {
      calendarOpError = formatError(e) || 'Failed to change color'
    } finally {
      calendarOpBusy = false
    }
  }

  async function commitNewCalendar() {
    if (!newCalendarForm || calendarOpBusy) return
    const { ncId, displayName, color } = newCalendarForm
    const trimmed = displayName.trim()
    if (!trimmed) return
    calendarOpBusy = true
    try {
      await api.calendar.createNextcloudCalendar({
        ncId,
        displayName: trimmed,
        color,
      })
      newCalendarForm = null
      await refreshCalendars()
    } catch (e) {
      calendarOpError = formatError(e) || 'Failed to create calendar'
    } finally {
      calendarOpBusy = false
    }
  }

  async function confirmCalendarDelete() {
    if (!deleteCalendarConfirm || calendarOpBusy) return
    const { id } = deleteCalendarConfirm
    calendarOpBusy = true
    try {
      await api.calendar.deleteNextcloudCalendar({ calendarId: id })
      deleteCalendarConfirm = null
      await refreshCalendars()
    } catch (e) {
      calendarOpError = formatError(e) || 'Failed to delete calendar'
    } finally {
      calendarOpBusy = false
    }
  }

  async function toggleCalendarMuted(c: CalendarSummary) {
    const newMuted = !c.muted
    // Optimistic update so the UI responds instantly.
    const idx = calendars.findIndex((cal) => cal.id === c.id)
    if (idx !== -1) calendars[idx] = { ...calendars[idx], muted: newMuted }
    try {
      await api.calendar.setNextcloudCalendarMuted({ calendarId: c.id, muted: newMuted })
    } catch (e) {
      // Rollback on failure.
      const i = calendars.findIndex((cal) => cal.id === c.id)
      if (i !== -1) calendars[i] = { ...calendars[i], muted: !newMuted }
      calendarOpError = formatError(e) || 'Failed to update calendar visibility'
    }
  }

  // ── Event editor state ──────────────────────────────────────
  // The editor is mounted lazily — only when one of these holds a
  // non-null value. Only one is ever set at a time (see openEditor /
  // openCreate / closeEditor). This keeps the modal a single thing on
  // screen no matter how it was triggered.
  let editingEvent = $state<CalendarEvent | null>(null)
  let creatingDraft = $state<{
    calendarId: string
    start: Date
    end: Date
    allDay?: boolean
  } | null>(null)

  // ── Click-and-drag-to-create state ──────────────────────────
  // Tracks an in-progress drag inside one of the day columns. While a
  // drag is active we render a translucent overlay block that follows
  // the pointer, and on mouseup we open the editor with the swept
  // range as the draft. Confined to a single day — multi-day drags are
  // a follow-up.
  type DragState = {
    dayKey: string
    startMinute: number
    currentMinute: number
    /** Event id the drag started on top of, if any.  Lets a
     *  no-movement mouseup fall back to "open this event"
     *  while still allowing real drags that originate inside
     *  an event block to create a new draft. */
    startEventId: string | null
    /** Raw clientY (px) of the mousedown — kept alongside the
     *  snapped `startMinute` so we can tell a real drag from a
     *  jittery click that just happened to cross a 15-minute
     *  snap boundary (#236).  Without this, pressing on an
     *  event near :07 and releasing near :08 would snap from
     *  minute 0 to 15 and be treated as a drag, creating a
     *  ghost event the user didn't ask for. */
    startClientY: number
    /** True once the cursor has moved past `CLICK_PIXEL_SLOP`
     *  pixels from `startClientY` (#236 follow-up).  Gates
     *  the drag-overlay render so a plain click doesn't
     *  flash a 2-pixel sliver of "you're creating an event"
     *  preview between mousedown and mouseup. */
    hasMoved: boolean
  }
  let drag = $state<DragState | null>(null)
  /** Pixel slop for the "this was a click, not a drag" check
   *  (#236).  4px matches the OS-level click vs drag threshold
   *  most platforms use; keeps the snapped-minute logic in
   *  place for genuine sweeping drags while letting clicks on
   *  an event block always open it for edit, even when they
   *  land right on a snap boundary. */
  const CLICK_PIXEL_SLOP = 4

  // Derived: calendar id → colour (for the coloured stripe on each
  // event block and the all-day pills).
  const colorById = $derived.by(() => {
    const m = new Map<string, string>()
    for (const c of calendars) {
      if (c.color) m.set(c.id, c.color)
    }
    return m
  })

  // Derived: the seven Date objects (one per column) for the current
  // week. Monday → Sunday.
  const weekDays = $derived.by<Date[]>(() => {
    const out: Date[] = []
    for (let i = 0; i < 7; i++) {
      const d = new Date(currentWeekStart)
      d.setDate(d.getDate() + i)
      out.push(d)
    }
    return out
  })

  // Derived: events filtered to the current week, bucketed by day
  // and by all-day-ness. `timed` events get absolute layout inside
  // the grid; `allDay` events stack in the header strip.
  type PlacedEvent = {
    event: CalendarEvent
    topPx: number
    heightPx: number
    lane: number
    laneCount: number
  }
  type WeekBucket = {
    date: Date
    dayKey: string
    timed: PlacedEvent[]
    allDay: CalendarEvent[]
  }
  const weekBuckets = $derived.by<WeekBucket[]>(() => {
    const buckets: WeekBucket[] = weekDays.map((d) => ({
      date: d,
      dayKey: dayKey(d),
      timed: [],
      allDay: [],
    }))
    // Index for O(1) push-by-day-key.
    const byKey = new Map<string, WeekBucket>()
    for (const b of buckets) byKey.set(b.dayKey, b)

    for (const ev of events) {
      // "Uncheck a calendar to hide its events" — drop events from
      // any calendar the user has toggled off before the (more
      // expensive) date math.
      if (mutedCalendarIds.has(eventCalendarId(ev))) continue
      const start = new Date(ev.start)
      const end = new Date(ev.end)
      if (isAllDay(start, end)) {
        // All-day events live on UTC calendar days (the backend stores
        // them as `00:00:00Z`–`23:59:59Z`). Use `end - 1ms` as the
        // inclusive-end boundary so a DURATION:P1D event (end at
        // midnight UTC of the next day, exclusive per RFC 5545) doesn't
        // leak onto that next day's bucket. Also covers the DTEND
        // fix-pending case if a sync hasn't refreshed the cache yet.
        // dayKey is `YYYY-MM-DD`, so lexicographic compare doubles as
        // date compare.
        const startKey = utcDayKey(start)
        const endKey = utcDayKey(new Date(end.getTime() - 1))
        for (const bucket of buckets) {
          if (bucket.dayKey >= startKey && bucket.dayKey <= endKey) {
            bucket.allDay.push(ev)
          }
        }
        continue
      }
      // Timed event: place into the bucket that holds its start day.
      // Events that cross midnight just render in the starting day
      // up to 23:59 for now — multi-day non-all-day events are rare
      // enough (overnight shifts, flights) that a dedicated follow-up
      // can refine this without changing the data model.
      const bucket = byKey.get(dayKey(start))
      if (!bucket) continue
      const dayStart = startOfDay(bucket.date)
      const dayEnd = endOfDay(bucket.date)
      const clampedStart = start < dayStart ? dayStart : start
      const clampedEnd = end > dayEnd ? dayEnd : end
      const topPx =
        (minutesBetween(dayStart, clampedStart)) * PX_PER_MINUTE
      const heightPx = Math.max(
        // Enforce a minimum height so a 0-minute point event still
        // has a visible block to click on.
        20,
        minutesBetween(clampedStart, clampedEnd) * PX_PER_MINUTE,
      )
      bucket.timed.push({
        event: ev,
        topPx,
        heightPx,
        lane: 0,
        laneCount: 1,
      })
    }

    // Cluster-aware lane assignment per day.
    //
    // Two passes:
    //
    // 1. Walk events in start-time order and group consecutive
    //    overlapping events into a "cluster". An event joins the
    //    current cluster when its start is before the cluster's
    //    running bottom (the latest end-time anyone in the cluster
    //    reaches); otherwise it opens a new cluster. Standalone
    //    events become single-event clusters.
    //
    // 2. Inside each cluster, assign each event to the lowest-numbered
    //    lane whose previous occupant has already ended — the standard
    //    overlap-packing algorithm. The cluster's lane count becomes
    //    the width divisor *for that cluster's events only*, so a
    //    15:30 event with no neighbours stays full width even on a
    //    day where 08:00 events are sharing two lanes.
    for (const bucket of buckets) {
      bucket.timed.sort((a, b) => a.topPx - b.topPx)

      type Cluster = { events: typeof bucket.timed; bottom: number }
      const clusters: Cluster[] = []
      for (const p of bucket.timed) {
        const last = clusters[clusters.length - 1]
        if (last && p.topPx < last.bottom) {
          last.events.push(p)
          last.bottom = Math.max(last.bottom, p.topPx + p.heightPx)
        } else {
          clusters.push({ events: [p], bottom: p.topPx + p.heightPx })
        }
      }

      for (const cluster of clusters) {
        const laneEnds: number[] = [] // each lane's current bottom (px)
        for (const p of cluster.events) {
          const bottom = p.topPx + p.heightPx
          let assigned = -1
          for (let i = 0; i < laneEnds.length; i++) {
            if (laneEnds[i] <= p.topPx) {
              assigned = i
              laneEnds[i] = bottom
              break
            }
          }
          if (assigned === -1) {
            assigned = laneEnds.length
            laneEnds.push(bottom)
          }
          p.lane = assigned
        }
        const laneCount = Math.max(1, laneEnds.length)
        for (const p of cluster.events) {
          p.laneCount = laneCount
        }
      }
    }

    return buckets
  })

  $effect(() => {
    void init()
  })

  /** Lower-cased addresses we consider "the user".  Drives the
   *  "you declined this event" detection — when an event has
   *  an ATTENDEE row whose email matches one of these AND the
   *  row's PARTSTAT is DECLINED, the grid renders it with the
   *  declined visual treatment (transparent fill, coloured
   *  border) instead of the regular filled block.  Loaded once
   *  on init from `get_accounts` (Unkai mail-account emails);
   *  empty by default so events still render correctly when
   *  the lookup fails. */
  let userIdentities = $state<Set<string>>(new Set())

  async function init() {
    loading = true
    error = ''
    try {
      accounts = await api.nextcloud.getNextcloudAccounts()
      if (accounts.length === 0) {
        error = 'Connect a Nextcloud account first to sync calendars.'
        loading = false
        return
      }
      // Best-effort identity load — failure leaves the set
      // empty and "user-declined" detection silently no-ops
      // (events render normally).
      try {
        const mailAccounts = await api.accounts.getAccounts()
        const set = new Set<string>()
        for (const a of mailAccounts) {
          if (a.email) set.add(a.email.toLowerCase())
        }
        userIdentities = set
      } catch (e) {
        console.warn('CalendarView: identity load failed', e)
      }
      const now = new Date()
      const start = addDays(now, -INITIAL_PAST_DAYS)
      const end = addDays(now, INITIAL_FUTURE_DAYS)
      await reloadFromCache(start, end)
    } catch (e) {
      error = formatError(e) || 'Failed to load calendars'
    } finally {
      loading = false
    }
    // Background sync for anything new server-side. Completes silently
    // except for the banner below the header when errors occur.
    void syncInBackground()
  }

  /** True when the user's own ATTENDEE row on this event
   *  carries `PARTSTAT=DECLINED`.  Drives the declined
   *  visual treatment in the grid — transparent fill with a
   *  calendar-coloured border so the slot is visible but
   *  clearly de-committed. */
  function userDeclined(ev: CalendarEvent): boolean {
    return userPartstatIs(ev, 'DECLINED')
  }
  /** True when the user's own ATTENDEE row carries
   *  `PARTSTAT=TENTATIVE`.  Drives the tentative visual
   *  (diagonal stripes).  Sourced from PARTSTAT — not from the
   *  event's `TRANSP` property — because TRANSP is event-level
   *  metadata that gets overwritten by CalDAV sync after our
   *  surgical RSVP PUT (we only edit the ATTENDEE row, not
   *  TRANSP), so the stripe class would otherwise drop off as
   *  soon as the next sync round-trips the body.  PARTSTAT is
   *  stored on the user's ATTENDEE row server-side and survives
   *  every sync round-trip. */
  function userTentative(ev: CalendarEvent): boolean {
    return userPartstatIs(ev, 'TENTATIVE')
  }
  function userPartstatIs(ev: CalendarEvent, want: string): boolean {
    if (userIdentities.size === 0) return false
    for (const a of ev.attendees ?? []) {
      if (
        userIdentities.has(a.email.toLowerCase()) &&
        (a.status ?? '').toUpperCase() === want
      ) {
        return true
      }
    }
    return false
  }

  /** Pull the first plausible meeting URL out of `URL`,
   *  `LOCATION`, or `DESCRIPTION` — same matcher as the
   *  backend's `extract_meeting_url`, just inline so the camera
   *  join button doesn't need an IPC round-trip per event.  Any
   *  `http(s)://…` token wins; we don't try to whitelist
   *  conferencing platforms because that bitrots quickly. */
  function extractMeetingUrl(ev: CalendarEvent): string | null {
    function fromField(s: string | null | undefined): string | null {
      if (!s) return null
      for (const tok of s.split(/\s+/)) {
        const url = tok.replace(/^[<("'.,;]+|[>)"'.,;]+$/g, '')
        if (url.startsWith('http://') || url.startsWith('https://')) return url
      }
      return null
    }
    return fromField(ev.url) ?? fromField(ev.location) ?? fromField(ev.description)
  }
  /** True when the current clock is within the ±5-minute join
   *  window around the event's start, so the camera button only
   *  appears when it's actually useful (5 min ahead = "drop in
   *  early"; 5 min after = "I just joined late, where's the
   *  link?").  Reactive on `now`, so the button auto-shows /
   *  hides as time passes without needing an explicit refresh. */
  const FIVE_MIN_MS = 5 * 60 * 1000
  function inJoinWindow(ev: CalendarEvent): boolean {
    const start = new Date(ev.start).getTime()
    const t = now.getTime()
    return t >= start - FIVE_MIN_MS && t <= start + FIVE_MIN_MS
  }
  /** Label rendered under the time line on each event block.
   *  Replaces a URL-shaped LOCATION (or an empty LOCATION on
   *  events whose URL/DESCRIPTION carry a meeting link) with
   *  the word "Online" — clearer than a 200-character Zoom
   *  URL crammed into a 110-pixel column.  Returns `null` when
   *  the event has no useful location to surface, so the
   *  caller can elide the line entirely. */
  function displayLocation(ev: CalendarEvent): string | null {
    const loc = (ev.location ?? '').trim()
    if (loc) {
      if (/^https?:\/\//i.test(loc)) return 'Online'
      return loc
    }
    if (extractMeetingUrl(ev)) return 'Online'
    return null
  }

  async function reloadFromCache(windowStart: Date, windowEnd: Date) {
    // Collect cached calendars across every connected NC account so a
    // user with multiple Nextclouds sees everything overlaid on one
    // grid.
    const allCalendars: CalendarSummary[] = []
    for (const a of accounts) {
      try {
        const cs = await api.calendar.getCachedCalendars({
          ncId: a.id,
        })
        allCalendars.push(...cs)
      } catch (e) {
        console.warn('get_cached_calendars failed for', a.id, e)
      }
    }
    calendars = allCalendars
    if (allCalendars.length === 0) {
      events = []
      loadedRangeStart = windowStart
      loadedRangeEnd = windowEnd
      return
    }
    try {
      events = await api.calendar.getCachedEvents({
        calendarIds: allCalendars.map((c) => c.id),
        rangeStart: windowStart.toISOString(),
        rangeEnd: windowEnd.toISOString(),
      })
      loadedRangeStart = windowStart
      loadedRangeEnd = windowEnd
    } catch (e) {
      console.warn('get_cached_events failed:', e)
      events = []
    }
  }

  async function syncInBackground() {
    if (syncing) return
    syncing = true
    syncErrors = []
    try {
      for (const a of accounts) {
        try {
          const report = await api.calendar.syncNextcloudCalendars(
            { ncId: a.id },
          )
          if (report.errors.length > 0) {
            syncErrors.push(...report.errors)
          }
        } catch (e) {
          console.warn('sync_nextcloud_calendars failed for', a.id, e)
          syncErrors.push(formatError(e) || `Sync failed for ${a.id}`)
        }
      }
      // Re-query the same window — fresh events from the sync appear
      // without the user doing anything.
      await reloadFromCache(loadedRangeStart, loadedRangeEnd)
    } finally {
      syncing = false
    }
  }

  // ── Navigation ──────────────────────────────────────────────
  function goToToday() {
    void navigateTo(startOfWeek(new Date()))
  }
  function prevWeek() {
    void navigateTo(addDays(currentWeekStart, -7))
  }
  function nextWeek() {
    void navigateTo(addDays(currentWeekStart, 7))
  }

  /** Move the visible week, extending the cached window on demand. */
  async function navigateTo(target: Date) {
    currentWeekStart = target
    const weekEnd = addDays(target, 7)
    // Extend if we're within the threshold of either edge — this runs
    // *before* the user can notice a gap, because the derived
    // `weekBuckets` already filters `events` to the visible week.
    const needsPast =
      daysBetween(loadedRangeStart, target) < EXTEND_THRESHOLD_DAYS
    const needsFuture =
      daysBetween(weekEnd, loadedRangeEnd) < EXTEND_THRESHOLD_DAYS
    if (!needsPast && !needsFuture) return
    const newStart = needsPast
      ? addDays(loadedRangeStart, -EXTEND_CHUNK_DAYS)
      : loadedRangeStart
    const newEnd = needsFuture
      ? addDays(loadedRangeEnd, EXTEND_CHUNK_DAYS)
      : loadedRangeEnd
    await reloadFromCache(newStart, newEnd)
  }

  // ── Formatting & geometry helpers ───────────────────────────

  function startOfWeek(d: Date): Date {
    const out = new Date(d.getFullYear(), d.getMonth(), d.getDate())
    // JS `getDay()` returns 0 for Sunday. We want ISO (Mon=1), so
    // shift back to the previous Monday — or today if it *is* Monday.
    const jsDay = out.getDay() // 0=Sun … 6=Sat
    const isoDay = jsDay === 0 ? 7 : jsDay // 1=Mon … 7=Sun
    const diff = isoDay - WEEK_STARTS_ON
    out.setDate(out.getDate() - diff)
    return out
  }
  function startOfDay(d: Date): Date {
    return new Date(d.getFullYear(), d.getMonth(), d.getDate())
  }
  function endOfDay(d: Date): Date {
    return new Date(d.getFullYear(), d.getMonth(), d.getDate() + 1)
  }
  function addDays(d: Date, n: number): Date {
    const out = new Date(d)
    out.setDate(out.getDate() + n)
    return out
  }
  function daysBetween(a: Date, b: Date): number {
    return Math.round((b.getTime() - a.getTime()) / 86400_000)
  }
  function minutesBetween(a: Date, b: Date): number {
    return (b.getTime() - a.getTime()) / 60_000
  }
  function dayKey(d: Date): string {
    const y = d.getFullYear()
    const m = String(d.getMonth() + 1).padStart(2, '0')
    const dd = String(d.getDate()).padStart(2, '0')
    return `${y}-${m}-${dd}`
  }
  function isAllDay(s: Date, e: Date): boolean {
    // The backend (`unkai-caldav::ical`) stores `VALUE=DATE` events
    // as midnight UTC → 23:59:59 UTC of the last covered day. Detect
    // on the UTC fields, not local ones — otherwise a user east of UTC
    // sees the event start at e.g. 02:00 (CEST) and isAllDay returns
    // false, dropping the event into the timed grid as a 22-hour block.
    if (s.getUTCHours() !== 0 || s.getUTCMinutes() !== 0) return false
    const hours = (e.getTime() - s.getTime()) / 3600_000
    if (hours < 23) return false
    // All-day shapes we actually observe in unkai-caldav output:
    //   - DURATION:P1D×N  → span is exactly N×24h
    //   - DTSTART only    → span is 23h59m59s (start + 86399s fallback)
    //   - DTEND snapped   → span is N×24h - 1s (ical.rs 23:59:59 snap)
    // All three sit within a minute of a whole-day boundary, so check
    // that the remainder is close to 0 *or* close to 24.
    const remainder = hours % 24
    return remainder < 1 / 60 || remainder > 24 - 1 / 60
  }

  /** UTC `YYYY-MM-DD` for `d`. Use when matching all-day events (UTC
      midnight by convention) against a bucket's calendar day. */
  function utcDayKey(d: Date): string {
    const y = d.getUTCFullYear()
    const m = String(d.getUTCMonth() + 1).padStart(2, '0')
    const dd = String(d.getUTCDate()).padStart(2, '0')
    return `${y}-${m}-${dd}`
  }

  function fmtTime(iso: string): string {
    return new Date(iso).toLocaleTimeString(undefined, {
      hour: '2-digit',
      minute: '2-digit',
    })
  }

  function weekRangeLabel(): string {
    const first = weekDays[0]
    const last = weekDays[6]
    const sameMonth = first.getMonth() === last.getMonth()
    const sameYear = first.getFullYear() === last.getFullYear()
    const fmtFull = (d: Date) =>
      d.toLocaleDateString(undefined, {
        month: 'short',
        day: 'numeric',
        year: 'numeric',
      })
    const fmtShort = (d: Date) =>
      d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
    const fmtDayOnly = (d: Date) =>
      d.toLocaleDateString(undefined, { day: 'numeric' })
    if (sameYear && sameMonth) {
      return `${fmtShort(first)} – ${fmtDayOnly(last)}, ${first.getFullYear()}`
    }
    if (sameYear) {
      return `${fmtShort(first)} – ${fmtShort(last)}, ${first.getFullYear()}`
    }
    return `${fmtFull(first)} – ${fmtFull(last)}`
  }

  function isToday(d: Date): boolean {
    const now = new Date()
    return (
      d.getDate() === now.getDate() &&
      d.getMonth() === now.getMonth() &&
      d.getFullYear() === now.getFullYear()
    )
  }

  /**
   * True for any calendar day strictly before today (in the user's local
   * timezone). Used to grey out past columns so the user can tell at a
   * glance which part of the week has already happened.
   *
   * Why local time, not UTC: the visible grid is rendered in local time
   * — a Tuesday column shows the user's local Tuesday, even if the user
   * is east of UTC and the day starts as Monday in UTC. The "past day"
   * decision needs to match what the user sees.
   */
  function isPast(d: Date): boolean {
    const now = new Date()
    const today = new Date(now.getFullYear(), now.getMonth(), now.getDate())
    const day = new Date(d.getFullYear(), d.getMonth(), d.getDate())
    return day.getTime() < today.getTime()
  }

  function eventCalendarId(ev: CalendarEvent): string {
    // Event ids come out of the store as `{nc_id}::{cal_path}::{uid}`
    // or the expanded `{…}::occ::{epoch}` form. Calendar ids are the
    // first two `::`-delimited segments. Slice those off either shape
    // to recover the owning calendar id.
    const parts = ev.id.split('::')
    return parts.slice(0, 2).join('::')
  }
  function eventColor(ev: CalendarEvent): string {
    return colorById.get(eventCalendarId(ev)) ?? '#2bb0ed'
  }

  // All-day overflow: if a day has more than the visible cap, show
  // the first N and a "+M more" affordance. Collapsed detail UI is a
  // follow-up — today this just prevents the strip from growing
  // unbounded.
  function allDayVisible(list: CalendarEvent[]): CalendarEvent[] {
    return list.slice(0, ALL_DAY_VISIBLE_ROWS)
  }
  function allDayOverflow(list: CalendarEvent[]): number {
    return Math.max(0, list.length - ALL_DAY_VISIBLE_ROWS)
  }

  // ── Editor open / close ─────────────────────────────────────
  /** Pick a sensible default calendar for a fresh `+ New event`.
      Prefers a calendar that is writable, sidebar-visible, and not
      muted; then a writable calendar regardless of mute; then any
      writable calendar at all.  Read-only calendars are skipped
      (#236) so the EventEditor's create-mode picker — which
      filters them out — never lands on a value missing from its
      own options list. */
  function defaultCalendarId(): string {
    for (const c of sidebarCalendars) {
      if (!c.muted && !c.read_only) return c.id
    }
    for (const c of sidebarCalendars) {
      if (!c.read_only) return c.id
    }
    return calendars.find((c) => !c.read_only)?.id ?? ''
  }

  function openCreateBlank() {
    if (calendars.length === 0) return
    const start = new Date()
    // Round to the next half-hour so the prefilled time looks
    // intentional rather than "11:37".
    start.setMinutes(start.getMinutes() < 30 ? 30 : 60, 0, 0)
    const end = new Date(start)
    end.setHours(end.getHours() + 1)
    creatingDraft = {
      calendarId: defaultCalendarId(),
      start,
      end,
    }
    editingEvent = null
  }

  function openEditor(ev: CalendarEvent) {
    editingEvent = ev
    creatingDraft = null
  }

  /** Honour `focusEventId` from the parent (#203 reminder card's
   *  "Show event" action).  Runs whenever either the prop or
   *  the loaded `events` list changes — covers both the case
   *  where the user clicks "Show event" while CalendarView is
   *  already mounted (events already loaded) and the case
   *  where the parent flips to `currentView = 'calendar'` and
   *  CalendarView mounts fresh (events arrive after the
   *  initial fetch resolves).  Looks first in `events` for an
   *  occurrence match (id ends with `::occ::<epoch>` for
   *  recurrence expansions); falls back to a UID-prefix match
   *  so the master event opens when the specific occurrence
   *  hasn't been expanded into the current window. */
  $effect(() => {
    if (!focusEventId) return
    if (events.length === 0) return
    let match = events.find((e) => e.id === focusEventId)
    if (!match) {
      // Try UID-prefix match — `{nc}::{cal}::{uid}` is a prefix
      // of `{nc}::{cal}::{uid}::occ::{epoch}`, so the master
      // catches both directions.
      const parts = focusEventId.split('::')
      if (parts.length >= 3) {
        const prefix = parts.slice(0, 3).join('::')
        match = events.find((e) => e.id.startsWith(prefix))
      }
    }
    if (match) {
      openEditor(match)
      oneventfocused?.()
    }
  })

  function closeEditor() {
    editingEvent = null
    creatingDraft = null
  }

  async function onEditorSaved(_saved?: SavedEvent) {
    // Re-query the loaded window so the new/updated/deleted event
    // shows up.  Outbound iMIP invites for events with attendees
    // are now sent server-side by Nextcloud's iMIP plugin (NC 30+
    // Mail Provider routes them through the user's own SMTP) —
    // nothing for the client to do beyond refreshing the grid.
    await reloadFromCache(loadedRangeStart, loadedRangeEnd)
  }

  // ── Click-and-drag in the time grid ─────────────────────────
  /** Map a vertical pixel offset inside a day column to a minute of
      the day, snapped to 15-minute increments so dragged events line
      up with the visual half-hour grid. */
  function pxToMinuteSnapped(yPx: number): number {
    const raw = Math.max(0, Math.min(24 * 60, yPx / PX_PER_MINUTE))
    return Math.round(raw / 15) * 15
  }

  /** Cursor offset inside the day column in the column's own
      *layout* pixels — the space `PX_PER_MINUTE` positions events
      in.  `clientY` and `getBoundingClientRect()` report *visual*
      pixels, which diverge from layout pixels whenever the UI-scale
      zoom (#191) is active; feeding the raw difference into
      `pxToMinuteSnapped` made drags land above/below the cursor by
      the zoom factor (#480).  Deriving the correction from the
      element itself (layout height ÷ visual height) stays right on
      every webview engine, whatever coordinate space it puts zoomed
      client coordinates in. */
  function columnLayoutY(ev: MouseEvent, el: HTMLElement): number {
    const rect = el.getBoundingClientRect()
    const ratio = rect.height > 0 ? el.offsetHeight / rect.height : 1
    return (ev.clientY - rect.top) * ratio
  }

  function onDayMouseDown(ev: MouseEvent, bucket: WeekBucket) {
    if (ev.button !== 0) return
    const target = ev.currentTarget as HTMLElement
    const minute = pxToMinuteSnapped(columnLayoutY(ev, target))
    // If the press lands inside an event block we still start a
    // drag — that lets the user sweep across an existing event
    // to create a new one in the same slot.  The event id is
    // remembered so a no-movement mouseup falls back to "open
    // this event" instead of creating a one-hour overlay.
    const evEl = (ev.target as HTMLElement).closest('[data-event-id]') as HTMLElement | null
    drag = {
      dayKey: bucket.dayKey,
      startMinute: minute,
      currentMinute: minute,
      startEventId: evEl?.dataset.eventId ?? null,
      startClientY: ev.clientY,
      hasMoved: false,
    }
  }

  function onDayMouseMove(ev: MouseEvent, bucket: WeekBucket) {
    if (!drag || drag.dayKey !== bucket.dayKey) return
    const target = ev.currentTarget as HTMLElement
    // Flip `hasMoved` only after the cursor crosses the
    // click-vs-drag threshold (#236) so the drag overlay
    // doesn't render — and the mouseup branch doesn't
    // create a ghost event — for what was really just a click.
    const pixelDelta = Math.abs(ev.clientY - drag.startClientY)
    drag = {
      ...drag,
      currentMinute: pxToMinuteSnapped(columnLayoutY(ev, target)),
      hasMoved: drag.hasMoved || pixelDelta > CLICK_PIXEL_SLOP,
    }
  }

  function onDayMouseUp(ev: MouseEvent, bucket: WeekBucket) {
    if (!drag || drag.dayKey !== bucket.dayKey) return
    const a = Math.min(drag.startMinute, drag.currentMinute)
    const b = Math.max(drag.startMinute, drag.currentMinute)
    // Pixel-distance check (#236): the snapped-minute test
    // alone treats a tiny mouse jiggle that crossed a 15-min
    // boundary as a drag, creating a phantom event the user
    // didn't ask for.  A press whose release lands within a
    // few pixels is a click no matter what the snapped
    // minutes say — open the event under the cursor and bail.
    const pixelDelta = Math.abs(ev.clientY - drag.startClientY)
    const isClick = pixelDelta <= CLICK_PIXEL_SLOP
    if (isClick && drag.startEventId) {
      const id = drag.startEventId
      drag = null
      const target = events.find((e) => e.id === id)
      if (target) openEditor(target)
      return
    }
    const moved = b - a >= 15 && !isClick
    // Click (no movement) on top of an existing event opens it
    // for editing — preserves the previous "click an event to
    // open it" affordance now that the event block no longer
    // swallows mousedown.
    if (!moved && drag.startEventId) {
      const id = drag.startEventId
      drag = null
      const target = events.find((e) => e.id === id)
      if (target) openEditor(target)
      return
    }
    // Otherwise: bare click on empty space → 1-hour event,
    // real drag → swept range.  Either way we open the editor
    // with a fresh draft.
    const startMinute = a
    const endMinute = moved ? b : a + 60
    const start = new Date(bucket.date)
    start.setHours(0, startMinute, 0, 0)
    const end = new Date(bucket.date)
    end.setHours(0, endMinute, 0, 0)
    drag = null
    creatingDraft = {
      calendarId: defaultCalendarId(),
      start,
      end,
    }
    editingEvent = null
  }

  function onDayMouseLeave() {
    // Cancel an in-flight drag if the cursor leaves the column —
    // otherwise mouseup over the sidebar would create an event in the
    // last column the user touched, which is surprising.
    drag = null
  }

  /** Geometry for the in-progress drag overlay rendered on the
      currently-active day column.  Returns `null` until the
      cursor has actually moved past `CLICK_PIXEL_SLOP` (#236):
      without this gate, a press-then-release click on an event
      tile briefly painted a 2px sliver of the create-event
      overlay between mousedown and mouseup which read as a
      flicker.  Real drags pass the threshold within the first
      mousemove tick so the overlay still appears responsively. */
  function dragOverlay(bucket: WeekBucket): { topPx: number; heightPx: number } | null {
    if (!drag || drag.dayKey !== bucket.dayKey || !drag.hasMoved) return null
    const a = Math.min(drag.startMinute, drag.currentMinute)
    const b = Math.max(drag.startMinute, drag.currentMinute)
    return {
      topPx: a * PX_PER_MINUTE,
      heightPx: Math.max(2, (b - a) * PX_PER_MINUTE),
    }
  }

  /** Listen for `calendars-updated` events from the backend
   *  (#236 follow-up).  The CalDAV write-failure fallback fires
   *  this whenever it flips a calendar's `read_only` flag in the
   *  cache after a 403/404 — re-pulling the calendar list here
   *  refreshes EventEditor's `calendars` prop so the
   *  `currentCalendarReadOnly` derived flips and Save/Delete
   *  hide on the in-flight editor instance. */
  $effect(() => {
    let unlisten: UnlistenFn | null = null
    let alive = true
    void (async () => {
      try {
        unlisten = await api.onAppEvent('calendars-updated', () => {
          if (!alive) return
          // Re-read the cache; events list isn't affected so the
          // existing window of events stays intact.
          void reloadFromCache(loadedRangeStart, loadedRangeEnd)
        })
      } catch (e) {
        console.warn('listen calendars-updated failed', e)
      }
    })()
    return () => {
      alive = false
      unlisten?.()
    }
  })
</script>

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Header -->
  <div
    class="flex items-center justify-between px-6 py-3 border-b glass-panel"
  >
    <div class="flex items-center gap-3">
      <h2 class="text-xl font-semibold">Calendar</h2>
    </div>
    <div class="flex items-center gap-2">
      <!-- Icon-only header actions matching the canonical shape
           used by SharesView / TalkView / FilesView.  "New event"
           stays filled-primary because it's the page's primary
           CTA; Sync sits next to it as the secondary action with
           the tonal-surface shape.  Sync swaps its glyph to
           `loading` while a background sync is in flight, mirroring
           how the other views' refresh buttons telegraph state. -->
      <button
        class="btn btn-sm preset-filled-primary-500 inline-flex items-center justify-center"
        disabled={calendars.length === 0}
        onclick={openCreateBlank}
        title={m.calendar_view_new_event_title()}
        aria-label={m.calendar_view_new_event()}
      ><Icon name="plus" size={14} /></button>
      <button
        class="btn btn-sm preset-tonal-surface inline-flex items-center justify-center"
        disabled={accounts.length === 0 || syncing}
        onclick={() => void syncInBackground()}
        title={syncing ? m.calendar_view_syncing() : m.calendar_view_sync_title()}
        aria-label={syncing ? m.calendar_view_syncing() : m.calendar_view_sync()}
      ><Icon name={syncing ? 'loading' : 'sync'} size={14} /></button>
    </div>
  </div>

  <!-- Body -->
  {#if loading}
    <p class="px-6 py-4 text-sm text-surface-500">Loading calendars…</p>
  {:else if error}
    <p class="px-6 py-4 text-sm text-red-500">{error}</p>
  {:else if calendars.length === 0}
    <p class="px-6 py-4 text-sm text-surface-500">
      No calendars cached yet. Open
      <strong>Settings → Nextcloud → Calendars</strong> and click
      <strong>Sync now</strong> to pull them from your Nextcloud account.
    </p>
  {:else}
    <div class="flex flex-1 min-h-0">
      <!-- Sidebar: per-calendar visibility toggles.
           Click a calendar to hide/show its events on the grid. The
           coloured swatch is the calendar's own colour from Nextcloud,
           so it matches the event blocks 1:1. -->
      <aside
        class="shrink-0 border-r border-surface-200 dark:border-surface-700 bg-surface-100/60 dark:bg-surface-800/40 flex flex-col"
        use:resizableSidebar={{ key: 'calendar.calendarsSidebar', defaultWidth: 224, min: 160, max: 480 }}
      >
        <!-- Inner scroll container so the aside-level resize handle
             stays pinned to the right edge instead of scrolling with
             the calendar list. -->
        <div class="flex-1 overflow-y-auto p-3">
        <!-- Section header. The `+` mirrors the Mail sidebar's
             "new folder" affordance so the add-calendar UX lives
             where the user already expects to look for it. Clicking
             the coloured swatch toggles a calendar's visibility
             inline — filled means visible, outlined means hidden. -->
        <div class="flex items-center justify-between mb-2 px-1">
          <span class="text-xs font-semibold uppercase tracking-wider text-surface-500">
            Calendars
          </span>
          <button
            class="w-5 h-5 rounded-lg flex items-center justify-center text-surface-500 hover:bg-primary-500/10 disabled:opacity-50"
            title="New calendar"
            aria-label="New calendar"
            disabled={calendarOpBusy || accounts.length === 0}
            onclick={() => {
              calendarContextMenu = null
              renamingCalendarId = null
              newCalendarForm = {
                ncId: accounts[0]?.id ?? '',
                displayName: '',
                color: COLOR_PRESETS[0],
              }
              calendarOpError = ''
            }}
          >+</button>
        </div>
        <ul class="space-y-1">
          {#each sidebarGroups as g, gi (g.ncId)}
            <!-- NC account divider + hostname header.  Skipped
                 above the very first group so the list doesn't
                 start with an empty divider line. -->
            <li class="pt-2 {gi === 0 ? '' : 'border-t border-surface-200 dark:border-surface-700 mt-2'}">
              <div
                class="px-1 pb-1 text-[10px] uppercase tracking-wider text-surface-500 truncate"
                title={g.label}
              >
                {g.label}
              </div>
            </li>
            {#each g.calendars as c (c.id)}
            <li>
              {#if renamingCalendarId === c.id}
                <div class="flex items-center gap-2 px-2 py-1">
                  <span
                    class="w-3 h-3 rounded-sm shrink-0"
                    style="background-color: {c.color ?? '#2bb0ed'};"
                  ></span>
                  <!-- svelte-ignore a11y_autofocus -->
                  <input
                    type="text"
                    class="input flex-1 text-sm px-2 py-0.5 rounded"
                    bind:value={calendarRenameValue}
                    disabled={calendarOpBusy}
                    autofocus
                    onkeydown={(e) => {
                      if (e.key === 'Enter') { e.preventDefault(); void commitCalendarRename() }
                      else if (e.key === 'Escape') { e.preventDefault(); cancelCalendarRename() }
                    }}
                    onblur={() => { if (renamingCalendarId) void commitCalendarRename() }}
                  />
                </div>
              {:else}
                <div
                  class="flex items-center gap-2 px-2 py-1 rounded hover:bg-surface-200/60 dark:hover:bg-surface-700/40 text-sm cursor-default"
                  role="listitem"
                  oncontextmenu={(e) => openCalendarContextMenu(e, c)}
                >
                  <!-- Swatch doubles as a one-click event-visibility toggle
                       (Layer 2). Filled = events visible, outlined = muted. -->
                  <button
                    class="w-3 h-3 rounded-sm shrink-0 border transition-colors cursor-pointer"
                    style={c.muted
                      ? `background-color: transparent; border-color: ${c.color ?? '#2bb0ed'};`
                      : `background-color: ${c.color ?? '#2bb0ed'}; border-color: ${c.color ?? '#2bb0ed'};`}
                    title={c.muted ? 'Show events' : 'Hide events'}
                    aria-label={c.muted ? 'Show events' : 'Hide events'}
                    onclick={(e) => { e.stopPropagation(); void toggleCalendarMuted(c) }}
                  ></button>
                  <span
                    class="flex-1 truncate {c.muted ? 'text-surface-400 dark:text-surface-500' : ''}"
                    title={c.display_name}
                  >
                    {c.display_name}
                  </span>
                </div>
              {/if}
            </li>
            {/each}
          {/each}
          {#if sidebarCalendars.length === 0 && calendars.length > 0}
            <li class="px-2 py-1 text-xs text-surface-500">
              All calendars are hidden. Toggle visibility in Settings.
            </li>
          {:else if calendars.length === 0}
            <li class="px-2 py-1 text-xs text-surface-500">
              No calendars yet. Add one with the + button above.
            </li>
          {/if}
        </ul>
        {#if calendarOpError}
          <p class="mt-2 px-1 text-xs text-red-500 wrap-break-word">{calendarOpError}</p>
        {/if}
        </div>
      </aside>

      <!-- Main grid area. Both header and time grid live in a single
           scrollable container with the header pinned via `sticky` so
           their column tracks share the same width and stay perfectly
           aligned no matter what the scrollbar does. `scrollbar-gutter:
           stable` reserves the scrollbar slot up front so the grid
           never reflows when content height crosses the viewport. -->
      <div class="flex-1 flex flex-col min-w-0">
        <!-- Week navigation, centered above the table.  Pinned at the
             top of the grid column so it sits between the section
             header and the day-of-week strip without scrolling away
             with the events. -->
        <div class="flex items-center justify-center gap-2 px-3 py-2 border-b border-surface-200 dark:border-surface-700">
          <button
            class="p-1.5 rounded-lg text-surface-600 dark:text-surface-300 hover:bg-primary-500/10 transition-colors"
            onclick={prevWeek}
            aria-label="Previous week"
            title="Previous week"
          >
            <Icon name="nav-backward" size={18} />
          </button>
          <button
            class="p-1.5 rounded-lg text-surface-600 dark:text-surface-300 hover:bg-primary-500/10 transition-colors"
            onclick={goToToday}
            aria-label="Jump to today"
            title="Jump to today"
          >
            <Icon name="today" size={18} />
          </button>
          <button
            class="p-1.5 rounded-lg text-surface-600 dark:text-surface-300 hover:bg-primary-500/10 transition-colors"
            onclick={nextWeek}
            aria-label="Next week"
            title="Next week"
          >
            <Icon name="nav-forward" size={18} />
          </button>
          <span class="text-sm font-medium ml-2 text-surface-700 dark:text-surface-300">{weekRangeLabel()}</span>
        </div>
        <div
          class="flex-1 overflow-y-auto"
          style="scrollbar-gutter: stable;"
        >
          <!-- Sticky day-of-week header. Each day cell carries the
               weekday label, the date number, and the all-day pills
               for that day stacked directly underneath — the standard
               "all-day events live with their date" layout. -->
          <div
            class="grid sticky top-0 z-10 border-b glass-panel"
            style="grid-template-columns: 56px repeat(7, minmax(0, 1fr));"
          >
            <div></div>
            {#each weekBuckets as b (b.dayKey)}
              {@const today = isToday(b.date)}
              {@const past = isPast(b.date)}
              <div
                class="px-2 py-2 text-center text-xs font-medium border-l border-surface-200 dark:border-surface-700 flex flex-col gap-1"
              >
                <div
                  class="uppercase tracking-wider"
                  class:text-surface-500={!past}
                  class:text-surface-400={past}
                >
                  {b.date.toLocaleDateString(undefined, { weekday: 'short' })}
                </div>
                <!--
                  Today's date number sits inside a red circle, the
                  common calendar-app convention. For past and future
                  days we drop the badge — past days additionally get
                  a muted text colour so the visited-vs-upcoming split
                  is obvious at a glance. We render the digit inside
                  an inline-flex'd span with fixed dimensions so the
                  badge stays a perfect circle regardless of digit
                  width (1 vs 31).
                -->
                <div class="flex justify-center leading-none">
                  {#if today}
                    <span
                      class="inline-flex items-center justify-center w-7 h-7 rounded-full bg-red-500 text-white text-base font-semibold"
                      aria-label="Today"
                    >
                      {b.date.getDate()}
                    </span>
                  {:else}
                    <span
                      class="text-lg font-semibold"
                      class:text-surface-400={past}
                    >
                      {b.date.getDate()}
                    </span>
                  {/if}
                </div>
                {#if b.allDay.length > 0}
                  <div class="flex flex-col gap-0.5 mt-1 text-left">
                    {#each allDayVisible(b.allDay) as ev (ev.id)}
                      <button
                        type="button"
                        class="ev-block ev-allday text-[11px] truncate rounded px-1.5 text-left {userTentative(ev) ? 'ev-tentative' : ''} {userDeclined(ev) ? 'ev-declined' : ''}"
                        style="--ev-color: {eventColor(ev)}; height: {ALL_DAY_ROW_HEIGHT_PX}px; line-height: {ALL_DAY_ROW_HEIGHT_PX}px;"
                        title={`${ev.summary || '(no title)'} — All-day${ev.location ? ` @ ${ev.location}` : ''}`}
                        onclick={() => openEditor(ev)}
                      >
                        {ev.summary || '(no title)'}
                      </button>
                    {/each}
                    {#if allDayOverflow(b.allDay) > 0}
                      <div class="text-[10px] text-surface-500">
                        +{allDayOverflow(b.allDay)} more
                      </div>
                    {/if}
                  </div>
                {/if}
              </div>
            {/each}
          </div>

          <!-- 24-hour time grid. -->
          <div
            class="relative grid"
            style="grid-template-columns: 56px repeat(7, minmax(0, 1fr)); height: {HOUR_HEIGHT_PX * 24}px;"
          >
            <!-- Hours axis. -->
            <div class="relative border-r border-surface-200 dark:border-surface-700">
              {#each Array.from({ length: 24 }, (_, i) => i) as h}
                <div
                  class="absolute left-0 right-0 text-[11px] text-surface-400 pr-2 text-right"
                  style="top: {h * HOUR_HEIGHT_PX}px;"
                >
                  {String(h).padStart(2, '0')}:00
                </div>
              {/each}
            </div>

            <!-- Seven day columns. Each one is its own relative container
                 so event blocks can absolutely-position against the
                 24-hour axis without interfering with each other. -->
            {#each weekBuckets as b (b.dayKey)}
              {@const overlay = dragOverlay(b)}
              {@const todayCol = isToday(b.date)}
              <div
                class="relative border-l border-surface-200 dark:border-surface-700 cursor-crosshair select-none"
                class:bg-surface-100={isPast(b.date)}
                class:dark:bg-surface-800={isPast(b.date)}
                onmousedown={(ev) => onDayMouseDown(ev, b)}
                onmousemove={(ev) => onDayMouseMove(ev, b)}
                onmouseup={(ev) => onDayMouseUp(ev, b)}
                onmouseleave={onDayMouseLeave}
                role="presentation"
              >
                <!-- Today: gray out the portion of the day that has
                     already happened, matching the full-column
                     background applied to past days.  Painted
                     *before* the gridlines so the hour rules still
                     show through it.  Pointer-events-none so the
                     user can still click-drag to create events in
                     the past part of today (they may want to log
                     something they just did). -->
                {#if todayCol}
                  <div
                    class="absolute left-0 right-0 top-0 bg-surface-100 dark:bg-surface-800 pointer-events-none"
                    style="height: {nowMinutes * PX_PER_MINUTE}px;"
                  ></div>
                {/if}

                <!-- Hour gridlines — pure visual rhythm.  Drawn
                     after the past-today overlay so the lines run
                     uninterrupted across the full 24 hours and the
                     greyed and ungreyed halves of today match. -->
                {#each Array.from({ length: 24 }, (_, i) => i) as h}
                  <div
                    class="absolute left-0 right-0 border-t border-surface-200/60 dark:border-surface-700/60"
                    style="top: {h * HOUR_HEIGHT_PX}px;"
                  ></div>
                {/each}

                {#if todayCol}
                  <!-- Current-time line. Red-500 + small leading
                       dot — the standard calendar-app indicator the
                       eye picks up immediately. -->
                  <div
                    class="absolute left-0 right-0 z-20 pointer-events-none"
                    style="top: {nowMinutes * PX_PER_MINUTE}px;"
                  >
                    <div class="absolute -left-1 -top-1 w-2 h-2 rounded-full bg-red-500"></div>
                    <div class="border-t border-red-500"></div>
                  </div>
                {/if}

                <!-- In-progress drag overlay — translucent rectangle
                     that follows the pointer between mousedown and
                     mouseup so the user can see the range they're
                     about to create. -->
                {#if overlay}
                  <div
                    class="absolute left-0.5 right-0.5 rounded-lg bg-primary-500/40 border border-primary-500 pointer-events-none"
                    style="top: {overlay.topPx}px; height: {overlay.heightPx}px;"
                  ></div>
                {/if}

                <!--
                  Event blocks. We keep the title (`<title>` tooltip) in
                  a "Name — HH:MM–HH:MM" shape so the time range is always
                  one hover away even when the block is short or the lane
                  is narrow. The visible label wraps onto multiple lines
                  (`break-words` + `whitespace-normal`) and we cap the
                  number of visible lines to whatever the block height
                  comfortably allows — short blocks get 1 line, taller
                  blocks get 2-3 — so the title shows as much as it can
                  without overflowing the block.
                -->
                {#each b.timed as p (p.event.id)}
                  {@const titleLineCap = p.heightPx >= 80
                    ? 4
                    : p.heightPx >= 50
                      ? 2
                      : 1}
                  {@const locationLabel = displayLocation(p.event)}
                  {@const meetingUrl = extractMeetingUrl(p.event)}
                  {@const showJoin = !!meetingUrl && inJoinWindow(p.event)}
                  <!-- Per-row visibility (#236): hide elements
                       that wouldn't fit instead of letting them
                       overflow.  Title always shows (line-
                       clamped), time shows when there's a second
                       row of space, and the location goes
                       through three states: full (pin + text),
                       pin-only as a hint that there *is* a
                       location worth expanding for, and hidden
                       when there's no room left at all.
                       Tile content top-aligns by default — the
                       tile reads as a normal calendar entry with
                       the title at the top.  Only when something
                       had to be hidden (time or location won't
                       fit) do we centre what's left, so a tiny
                       sliver shows its title in the middle of
                       the box rather than mashed against the
                       top edge. -->
                  {@const showTime = p.heightPx >= 32}
                  {@const showLocationFull = !!locationLabel && p.heightPx >= 50}
                  {@const showLocationIcon =
                    !!locationLabel && p.heightPx >= 38 && p.heightPx < 50}
                  {@const compactCentered =
                    !showTime || (!!locationLabel && !showLocationFull)}
                  <div
                    data-event-id={p.event.id}
                    class="ev-block ev-timed absolute rounded-lg text-[11px] overflow-hidden px-1.5 py-1 cursor-pointer leading-tight flex flex-col {compactCentered ? 'justify-center' : 'justify-start'} {userTentative(p.event) ? 'ev-tentative' : ''} {userDeclined(p.event) ? 'ev-declined' : ''}"
                    style="--ev-color: {eventColor(p.event)}; top: {p.topPx}px; height: {p.heightPx}px; left: calc({(p.lane / p.laneCount) * 100}% + 2px); width: calc({(1 / p.laneCount) * 100}% - 4px);"
                    title={`${p.event.summary || '(no title)'} — ${fmtTime(p.event.start)}–${fmtTime(p.event.end)}${p.event.location ? ` @ ${p.event.location}` : ''}`}
                    role="button"
                    tabindex="0"
                    onkeydown={(ev) => { if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); openEditor(p.event) } }}
                  >
                    <div
                      class="font-medium wrap-break-word"
                      style="display: -webkit-box; -webkit-line-clamp: {titleLineCap}; -webkit-box-orient: vertical; overflow: hidden;"
                    >
                      {p.event.summary || '(no title)'}
                    </div>
                    {#if showTime}
                      <div class="opacity-90 truncate">
                        {fmtTime(p.event.start)} – {fmtTime(p.event.end)}
                      </div>
                    {/if}
                    {#if showLocationFull}
                      <!-- Location line — sits directly under
                           the time so the "where" answer is
                           always one glance below the "when".
                           URL-shaped locations collapse to
                           "Online" so a 200-character Zoom
                           link doesn't trash a tight column.
                           Pin glyph leads the line as a quick
                           visual anchor (the common calendar-app
                           convention). -->
                      <div class="opacity-90 truncate flex items-center gap-1">
                        <svg
                          xmlns="http://www.w3.org/2000/svg"
                          class="w-3 h-3 shrink-0"
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="2"
                          stroke-linecap="round"
                          stroke-linejoin="round"
                          aria-hidden="true"
                        >
                          <path d="M21 10c0 7-9 13-9 13S3 17 3 10a9 9 0 1 1 18 0z" />
                          <circle cx="12" cy="10" r="3" />
                        </svg>
                        <span class="truncate">{locationLabel}</span>
                      </div>
                    {:else if showLocationIcon}
                      <!-- Pin-only fallback when the text won't
                           fit but a sliver of vertical space is
                           still available.  Hover tooltip on the
                           parent block carries the full location,
                           so this is a "there's a location worth
                           expanding for" hint rather than a
                           dead-end glyph. -->
                      <div class="opacity-90 leading-none">
                        <svg
                          xmlns="http://www.w3.org/2000/svg"
                          class="w-3 h-3"
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="2"
                          stroke-linecap="round"
                          stroke-linejoin="round"
                          aria-hidden="true"
                        >
                          <path d="M21 10c0 7-9 13-9 13S3 17 3 10a9 9 0 1 1 18 0z" />
                          <circle cx="12" cy="10" r="3" />
                        </svg>
                      </div>
                    {/if}
                    {#if showJoin}
                      <!-- Join button — only visible during the
                           ±5-minute window around the event's
                           start.  Sits at the bottom-right corner
                           so it doesn't crowd the title.  Stops
                           propagation so a click on the camera
                           opens the meeting URL instead of the
                           event editor. -->
                      <button
                        type="button"
                        class="absolute bottom-1 right-1 w-6 h-6 rounded-lg flex items-center justify-center bg-primary-500 text-white shadow hover:bg-primary-600 z-10"
                        title={`Join meeting (${meetingUrl})`}
                        aria-label="Join meeting"
                        onmousedown={(ev) => ev.stopPropagation()}
                        onclick={(ev) => {
                          ev.stopPropagation()
                          void api.system.openUrl({ url: meetingUrl }).catch((err) =>
                            console.warn('open_url failed', err),
                          )
                        }}
                      >
                        <svg
                          xmlns="http://www.w3.org/2000/svg"
                          class="w-3.5 h-3.5"
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="2.2"
                          stroke-linecap="round"
                          stroke-linejoin="round"
                          aria-hidden="true"
                        >
                          <path d="M23 7l-7 5 7 5V7z" />
                          <rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
                        </svg>
                      </button>
                    {/if}
                  </div>
                {/each}
              </div>
            {/each}
          </div>
        </div>

        {#if syncErrors.length > 0}
          <div
            class="mx-6 my-2 p-3 rounded-lg border border-red-200 dark:border-red-700 bg-red-50 dark:bg-red-950 text-xs text-red-700 dark:text-red-200 shrink-0"
          >
            <p class="font-semibold mb-1">Some calendars failed to sync:</p>
            <ul class="list-disc list-inside space-y-0.5">
              {#each syncErrors as msg}
                <li>{msg}</li>
              {/each}
            </ul>
          </div>
        {/if}
      </div>
    </div>
  {/if}
</div>

{#if creatingDraft}
  <!-- Only pass the *visible* calendars to the editor — the
       dropdown is how the user picks where a new event lands, and
       a hidden calendar shouldn't be a reachable destination. Edit
       mode is unreachable on hidden calendars anyway (their events
       never paint on the grid), so the same filter applies there. -->
  <EventEditor
    mode="create"
    calendars={visibleCalendars}
    draft={creatingDraft}
    onclose={closeEditor}
    onsaved={onEditorSaved}
  />
{:else if editingEvent}
  <EventEditor
    mode="edit"
    calendars={visibleCalendars}
    event={editingEvent}
    onclose={closeEditor}
    onsaved={onEditorSaved}
  />
{/if}

<!-- Right-click context menu for a calendar row. `position: fixed`
     at the click point, z-60 so it sits above modal overlays. -->
{#if calendarContextMenu}
  <div
    class="fixed z-60 min-w-44 rounded-xl glass-float py-1 text-sm"
    style="left: {Math.min(calendarContextMenu.x, window.innerWidth - 200)}px; top: {Math.min(calendarContextMenu.y, window.innerHeight - 150)}px;"
    role="menu"
    tabindex="-1"
    onmousedown={(e) => e.stopPropagation()}
  >
    <button
      class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={calendarOpBusy}
      onclick={() => {
        const c = calendarContextMenu!.calendar
        calendarContextMenu = null
        renamingCalendarId = c.id
        calendarRenameValue = c.display_name
      }}
    >Rename</button>
    <button
      class="w-full text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800 disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={calendarOpBusy}
      onclick={() => {
        const c = calendarContextMenu!.calendar
        calendarContextMenu = null
        colorPicker = { calendar: c, color: c.color ?? COLOR_PRESETS[0] }
      }}
    >Change color…</button>
    <button
      class="w-full text-left px-3 py-1.5 hover:bg-red-500/10 text-red-600 dark:text-red-400 disabled:opacity-50 disabled:hover:bg-transparent"
      disabled={calendarOpBusy}
      onclick={() => {
        const c = calendarContextMenu!.calendar
        calendarContextMenu = null
        deleteCalendarConfirm = c
      }}
    >Delete…</button>
  </div>
{/if}

<!-- New calendar modal. Name + color swatch grid + optional NC
     account picker (only rendered when the user has >1 connected). -->
{#if newCalendarForm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) newCalendarForm = null }}
  >
    <div class="glass-float rounded-2xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-3">New calendar</h3>

      <!-- Account picker: only meaningful with more than one
           connected NC account.  Hidden when there's a single
           account (the form's `ncId` is already pre-seeded with
           it on open). -->
      {#if accounts.length > 1}
        <label class="block text-xs text-surface-500 mb-1" for="new-cal-account">Nextcloud account</label>
        <div class="mb-3">
          <Select
            id="new-cal-account"
            ariaLabel="Nextcloud account"
            bind:value={newCalendarForm.ncId}
            disabled={calendarOpBusy}
            options={accounts.map((a) => ({
              value: a.id,
              label: `${a.display_name || a.username} — ${ncHostname(a.server_url)}`,
            }))}
          />
        </div>
      {/if}

      <label class="block text-xs text-surface-500 mb-1" for="new-cal-name">Name</label>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        id="new-cal-name"
        type="text"
        class="input w-full text-sm px-2 py-1.5 rounded-lg mb-3"
        placeholder="Work, Family, …"
        bind:value={newCalendarForm.displayName}
        disabled={calendarOpBusy}
        autofocus
        onkeydown={(e) => {
          if (e.key === 'Enter' && newCalendarForm?.displayName.trim()) {
            e.preventDefault()
            void commitNewCalendar()
          }
        }}
      />

      <!-- Color grid. Click-to-select; the chosen swatch shows a
           ring. Kept as a fixed palette — matches what Nextcloud
           web offers, and `MKCALENDAR` accepts any hex anyway if a
           user wants a custom tone (there's a native color input
           in the "Change color" flow for that). -->
      <div class="text-xs text-surface-500 mb-1">Color</div>
      <div class="flex flex-wrap gap-1.5 mb-4">
        {#each COLOR_PRESETS as swatch (swatch)}
          <button
            type="button"
            class="w-7 h-7 rounded-full transition-transform
                   {newCalendarForm.color === swatch
                     ? 'ring-2 ring-offset-2 ring-offset-surface-50 dark:ring-offset-surface-900 ring-primary-500'
                     : 'hover:scale-110'}"
            style="background-color: {swatch};"
            title={swatch}
            aria-label="Color {swatch}"
            onclick={() => { newCalendarForm!.color = swatch }}
          ></button>
        {/each}
      </div>

      {#if calendarOpError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{calendarOpError}</p>
      {/if}

      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={calendarOpBusy}
          onclick={() => (newCalendarForm = null)}
        >Cancel</button>
        <button
          class="btn preset-filled-primary-500"
          disabled={calendarOpBusy || !newCalendarForm.displayName.trim()}
          onclick={() => void commitNewCalendar()}
        >{calendarOpBusy ? 'Creating…' : 'Create'}</button>
      </div>
    </div>
  </div>
{/if}

<!-- Color picker modal. Presets + a native `<input type="color">`
     for anything outside the palette. -->
{#if colorPicker}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) colorPicker = null }}
  >
    <div class="glass-float rounded-2xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">Change color</h3>
      <p class="text-xs text-surface-500 mb-4">
        For <span class="font-medium text-surface-700 dark:text-surface-300">{colorPicker.calendar.display_name}</span>
      </p>

      <div class="flex flex-wrap gap-1.5 mb-4">
        {#each COLOR_PRESETS as swatch (swatch)}
          <button
            type="button"
            class="w-7 h-7 rounded-full transition-transform
                   {colorPicker.color === swatch
                     ? 'ring-2 ring-offset-2 ring-offset-surface-50 dark:ring-offset-surface-900 ring-primary-500'
                     : 'hover:scale-110'}"
            style="background-color: {swatch};"
            title={swatch}
            aria-label="Color {swatch}"
            onclick={() => { colorPicker!.color = swatch }}
          ></button>
        {/each}
      </div>

      <div class="flex items-center gap-2 mb-4">
        <label class="text-xs text-surface-500 shrink-0" for="color-picker-custom">Custom:</label>
        <input
          id="color-picker-custom"
          type="color"
          class="w-10 h-8 border border-surface-300 dark:border-surface-600 rounded"
          bind:value={colorPicker.color}
          disabled={calendarOpBusy}
        />
        <span class="text-xs text-surface-500 font-mono">{colorPicker.color}</span>
      </div>

      {#if calendarOpError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{calendarOpError}</p>
      {/if}

      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={calendarOpBusy}
          onclick={() => (colorPicker = null)}
        >Cancel</button>
        <button
          class="btn preset-filled-primary-500"
          disabled={calendarOpBusy}
          onclick={() => void commitColorChange()}
        >{calendarOpBusy ? 'Saving…' : 'Save'}</button>
      </div>
    </div>
  </div>
{/if}

<!-- Delete-calendar confirm. Destructive ops always pass through
     an explicit confirm because the server-side DELETE is
     irreversible on most Nextcloud setups. -->
{#if deleteCalendarConfirm}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) deleteCalendarConfirm = null }}
  >
    <div class="glass-float rounded-2xl w-96 max-w-full p-5">
      <h3 class="text-base font-semibold mb-2">Delete calendar?</h3>
      <p class="text-sm text-surface-700 dark:text-surface-300 mb-4">
        Delete <span class="font-medium">{deleteCalendarConfirm.display_name}</span> and every event in it?
        This can't be undone.
      </p>
      {#if calendarOpError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{calendarOpError}</p>
      {/if}
      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={calendarOpBusy}
          onclick={() => (deleteCalendarConfirm = null)}
        >Cancel</button>
        <button
          class="btn preset-filled-error-500"
          disabled={calendarOpBusy}
          onclick={() => void confirmCalendarDelete()}
        >{calendarOpBusy ? 'Deleting…' : 'Delete'}</button>
      </div>
    </div>
  </div>
{/if}

<style>
  /*
   * Modern event block: a translucent tint of the calendar colour
   * with a solid accent bar on the leading edge and saturated text
   * in the same hue. The background always uses the same CSS
   * custom property, so each event just sets `--ev-color: <hex>`
   * inline and the rest falls out.
   *
   * Why this shape:
   * - `color-mix(... <n>%, transparent)` gives us a tint that
   *   lets the grid lines underneath show through; adjacent events
   *   (even of the same colour) stay visibly distinct because the
   *   accent bar and saturated text make each block's edge
   *   unambiguous.
   * - We use full saturation for the accent bar and for the text,
   *   so Nextcloud-assigned colours read the same way they do in
   *   every other calendar client — yellow stays yellow, teal
   *   stays teal.
   * - The tint % is higher in dark mode (28 vs. 18) because
   *   `color-mix(..., transparent)` loses punch against a near-black
   *   surface; 18% looks ghostly in `[data-mode='dark']`.
   *
   * Browser support: `color-mix()` is shipping in Chrome 111+,
   * Safari 16.2+, Firefox 113+ — all well within Tauri 2's system
   * webview requirements.
   */
  .ev-block {
    background-color: color-mix(in srgb, var(--ev-color) 18%, transparent);
    color: var(--ev-color);
    border: 1px solid color-mix(in srgb, var(--ev-color) 35%, transparent);
    /* The inset box-shadow is the coloured left accent bar — the
       primary visual separator between stacked events, and why
       adjacent blocks of the same colour no longer blur together. */
    box-shadow: inset 3px 0 0 0 var(--ev-color);
    padding-left: 8px;
  }

  /* Dark mode: bump the tint so the block reads clearly against a
     near-black surface. Text + accent stay saturated in both modes
     — the full-colour hue is already high-contrast against a dark
     background. */
  :global([data-mode='dark']) .ev-block {
    background-color: color-mix(in srgb, var(--ev-color) 28%, transparent);
    border-color: color-mix(in srgb, var(--ev-color) 45%, transparent);
  }

  /* Subtle hover lift signals clickability without the old shadow
     that used to make adjacent blocks blur together. */
  .ev-block.ev-timed:hover,
  .ev-block.ev-allday:hover {
    background-color: color-mix(in srgb, var(--ev-color) 28%, transparent);
  }
  :global([data-mode='dark']) .ev-block.ev-timed:hover,
  :global([data-mode='dark']) .ev-block.ev-allday:hover {
    background-color: color-mix(in srgb, var(--ev-color) 38%, transparent);
  }

  /* Tentative events (TRANSP=TRANSPARENT — set by the RSVP card
     when the user picks Tentative on an inbound invite, and by
     anything else that opts into the "this slot might still be
     free" semantic).  Diagonal stripe pattern, layered on top of
     the regular tinted background, so the block is still readable
     but visually marked as "soft commitment".  Rendered via a
     repeating-linear-gradient on top of the base color-mix fill;
     the `8%` stripe is faint enough on light backgrounds and
     comes through subtly in dark mode too. */
  .ev-block.ev-tentative {
    background-image: repeating-linear-gradient(
      45deg,
      transparent 0,
      transparent 6px,
      color-mix(in srgb, var(--ev-color) 35%, transparent) 6px,
      color-mix(in srgb, var(--ev-color) 35%, transparent) 8px
    );
  }

  /* Declined events — the user RSVPed "no" but the meeting is
     still on the calendar (Apple Calendar's behaviour).  No
     fill so the block doesn't look like a real commitment, but
     a fully-coloured border so it's still readable + the
     calendar's identity colour stays visible.  Text de-emphasis
     (lower opacity + line-through) makes the "I'm not going"
     state unambiguous at a glance. */
  .ev-block.ev-declined {
    background: transparent !important;
    background-image: none !important;
    border: 1.5px solid var(--ev-color);
    color: color-mix(in srgb, var(--ev-color) 80%, currentColor);
    text-decoration: line-through;
    opacity: 0.85;
  }
  .ev-block.ev-declined:hover {
    background-color: color-mix(in srgb, var(--ev-color) 12%, transparent) !important;
    opacity: 1;
  }
  :global([data-mode='dark']) .ev-block.ev-declined:hover {
    background-color: color-mix(in srgb, var(--ev-color) 22%, transparent) !important;
  }
</style>
