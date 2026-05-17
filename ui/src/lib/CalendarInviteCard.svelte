<script lang="ts">
  /**
   * CalendarInviteCard — RSVP UI for inbound mail carrying a
   * `text/calendar; method=REQUEST` attachment (#58, iMIP).
   *
   * Surfaced by `MailView` whenever the open message has at least
   * one `text/calendar` attachment.  The card shows the invite's
   * title + time slot + location/url so the user can sanity-check
   * the slot before responding, then sends an RSVP via
   * `send_event_rsvp` (silent — no Compose modal).
   *
   * The full ICS bytes ride through `raw_ics` so the REPLY can
   * preserve the original UID and propagate the user's PARTSTAT
   * back without re-fetching anything.
   */

  import { invoke, convertFileSrc } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import Icon, { type IconName } from './Icon.svelte'

  /** Mirrors the Rust `EventAttendee` struct.  Optional fields
   *  are `null` over IPC; we treat them as missing. */
  interface InviteAttendee {
    email: string
    common_name?: string | null
    status?: string | null
  }

  /** Mirrors the Rust `InviteSummary` returned by
   *  `parse_event_invite`.  `rawIcs` is the original ICS body —
   *  passed back verbatim to `send_event_rsvp` so the REPLY
   *  carries the right UID / DTSTAMP / SEQUENCE. */
  export interface InviteSummary {
    /** Calendar-level METHOD (REQUEST / REPLY / CANCEL …).
     *  `MailView` filters on this to suppress the card for
     *  non-REQUEST messages. */
    method: string | null
    uid: string
    summary: string
    start: string
    end: string
    location: string | null
    url: string | null
    organizerEmail: string | null
    organizerName: string | null
    attendees: InviteAttendee[]
    rawIcs: string
  }

  let {
    invite,
    onresponded,
  }: {
    invite: InviteSummary
    /** Fires after a successful RSVP so the parent can
     *  optimistically update the mail's read state, hide the
     *  card, etc. */
    onresponded?: (partstat: 'ACCEPTED' | 'DECLINED' | 'TENTATIVE') => void
  } = $props()

  /** Calendar picker rows mirroring `CalendarSummary` — fetched
   *  from every connected Nextcloud account so the user can drop
   *  the accepted event into any of their calendars. */
  interface CalendarRow {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    hidden?: boolean
    /** Layer-2 visibility — set from the calendar sidebar's
     *  swatch toggle.  Muted calendars stay in the picker
     *  (the user might still want to drop the accepted event
     *  there), but their events are excluded from the day
     *  preview so the "what's already on my day" view matches
     *  what the user actually has visible in CalendarView. */
    muted?: boolean
    /** CalDAV-derived read-only flag (#236).  The accept-into
     *  picker filters these out — accepting an invite into a
     *  read-only calendar would fail server-side. */
    read_only?: boolean
  }
  /** Slim mirror of the Rust `CalendarEvent` — we only consume
   *  the fields needed to lay the preview out (id for calendar
   *  lookup + key, start/end for positioning, summary for the
   *  hover title). */
  interface PreviewEvent {
    id: string
    summary: string
    start: string
    end: string
    /** Attendee list — used to detect the current user's
     *  PARTSTAT so we can render existing events with the
     *  same tentative/declined treatment as the agenda grid. */
    attendees?: InviteAttendee[] | null
  }
  let calendars = $state<CalendarRow[]>([])
  let selectedCalendarId = $state<string | null>(null)
  let calendarsLoading = $state(true)
  /** Best-effort hint for the backend: the address from the
   *  inbound ATTENDEE list that matches one of the user's
   *  configured mail accounts.  Backend treats it as the
   *  highest-priority candidate when picking the row to mutate
   *  + identify with on Sabre's principal CUA.  Stays `null`
   *  when the invite uses an address Unkai doesn't have a
   *  matching account for; backend falls back to NC profile
   *  email + mail-account list. */
  let attendeeHint = $state<string | null>(null)

  /** Lower-cased email addresses identifying the current user.
   *  Loaded from the configured mail accounts on mount and
   *  used to find the user's own ATTENDEE row in any preview
   *  event so we can apply the same tentative / declined
   *  visuals the agenda grid uses. */
  let userIdentities = $state<Set<string>>(new Set())
  $effect(() => {
    void invoke<{ email: string }[]>('get_accounts')
      .then((rows) => {
        const set = new Set<string>()
        for (const r of rows) if (r.email) set.add(r.email.toLowerCase())
        userIdentities = set
      })
      .catch(() => {})
  })

  /** Return the user's PARTSTAT on an existing preview event,
   *  or `null` if the event has no attendee row matching the
   *  user — matches the semantics of CalendarView's
   *  `userPartstatIs`. */
  function userPartstatOn(ev: PreviewEvent): string | null {
    if (userIdentities.size === 0) return null
    for (const a of ev.attendees ?? []) {
      if (userIdentities.has(a.email.toLowerCase())) {
        return (a.status ?? '').toUpperCase() || null
      }
    }
    return null
  }

  // ── "More info" details panel ──────────────────────────────
  // Expanded view: read-only attendee chips + a one-day mini
  // preview showing what else is on the user's calendar(s) for
  // the proposed slot.  Loaded lazily on first expand so the
  // card stays cheap when the user just wants to Accept and
  // move on.
  let detailsOpen = $state(false)
  let previewLoading = $state(false)
  let previewError = $state('')
  /** Currently-displayed day in the preview.  Initialised to
   *  the invite's local day on first open; the prev/next day
   *  arrows mutate this without changing the invite itself.
   *  Reads `invite.start` via the locally-ignored hint so the
   *  Svelte 5 compiler knows we deliberately seed once and
   *  don't track invite updates here (the auto-scroll effect
   *  re-targets the slot whenever the invite uid changes). */
  // svelte-ignore state_referenced_locally
  let previewDate = $state<Date>(new Date(invite.start))
  /** Events keyed by `YYYY-MM-DD` so navigating back to a
   *  previously-loaded day is instant — IPC + expansion only
   *  hits the cache the first time the user lands on a date. */
  let previewEventsByDate = $state<Map<string, PreviewEvent[]>>(new Map())
  function dayKey(d: Date): string {
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
  }
  let previewEvents = $derived(previewEventsByDate.get(dayKey(previewDate)) ?? [])
  let previewLoaded = $derived(previewEventsByDate.has(dayKey(previewDate)))

  /** Map calendar id → row, for colour + display-name lookup
   *  in the preview block.  `eventCalendarId(ev.id)` recovers
   *  the calendar id from the composite event id. */
  const calendarsById = $derived.by(() => {
    const m = new Map<string, CalendarRow>()
    for (const c of calendars) m.set(c.id, c)
    return m
  })
  function eventCalendarId(id: string): string {
    return id.split('::').slice(0, 2).join('::')
  }

  async function loadPreviewForDate(d: Date) {
    const key = dayKey(d)
    if (previewEventsByDate.has(key) || previewLoading) return
    previewLoading = true
    previewError = ''
    try {
      const dayStart = new Date(d.getFullYear(), d.getMonth(), d.getDate())
      const dayEnd = new Date(dayStart)
      dayEnd.setDate(dayEnd.getDate() + 1)
      const activeIds = calendars
        .filter((c) => !c.hidden && !c.muted)
        .map((c) => c.id)
      const ev =
        activeIds.length === 0
          ? []
          : await invoke<PreviewEvent[]>('get_cached_events', {
              calendarIds: activeIds,
              rangeStart: dayStart.toISOString(),
              rangeEnd: dayEnd.toISOString(),
            })
      // Re-bind so Svelte's reactivity picks up the new entry.
      const next = new Map(previewEventsByDate)
      next.set(key, ev)
      previewEventsByDate = next
    } catch (e) {
      previewError = formatError(e) || 'Failed to load calendar preview'
    } finally {
      previewLoading = false
    }
  }

  function toggleDetails() {
    detailsOpen = !detailsOpen
    if (detailsOpen && !previewLoaded) void loadPreviewForDate(previewDate)
  }
  function shiftPreviewDay(deltaDays: number) {
    const next = new Date(
      previewDate.getFullYear(),
      previewDate.getMonth(),
      previewDate.getDate() + deltaDays,
    )
    previewDate = next
    if (!previewEventsByDate.has(dayKey(next))) void loadPreviewForDate(next)
  }

  // ── Mini day-grid layout ───────────────────────────────────
  // Full 24-hour grid, vertically scrollable.  The viewport caps
  // height at ~9 hours so the panel stays compact, and on first
  // expand we auto-scroll the proposed slot near the top of the
  // viewport — keeps overlapping events visible without forcing
  // the user to hunt for the meeting in question.
  const HOUR_PX = 32
  const PREVIEW_VIEWPORT_PX = 280
  const PREVIEW_TOTAL_PX = 24 * HOUR_PX

  /** Local-midnight of the currently-displayed day. */
  let previewDayStart = $derived(
    new Date(
      previewDate.getFullYear(),
      previewDate.getMonth(),
      previewDate.getDate(),
    ),
  )
  let previewDayEnd = $derived.by(() => {
    const d = new Date(previewDayStart)
    d.setDate(d.getDate() + 1)
    return d
  })
  function minutesFromDayStart(iso: string, dayStart: Date): number {
    return (new Date(iso).getTime() - dayStart.getTime()) / 60000
  }
  /** Top + height (px) on the full-day axis, clamped to the
   *  visible 24h window of `previewDayStart`.  Events that
   *  spill in from the previous day clamp to top:0, and events
   *  that bleed past midnight clamp to the bottom — instead of
   *  silently dropping. */
  function blockGeometry(
    startISO: string,
    endISO: string,
  ): { top: number; height: number } | null {
    const s = Math.max(0, minutesFromDayStart(startISO, previewDayStart))
    const e = Math.min(
      24 * 60,
      minutesFromDayStart(endISO, previewDayStart),
    )
    if (e <= s) return null
    return {
      top: (s / 60) * HOUR_PX,
      height: ((e - s) / 60) * HOUR_PX,
    }
  }
  /** True iff the invite's slot intersects the day currently
   *  being previewed.  When the user has navigated to a
   *  different day with the arrows, the proposed slot is
   *  hidden because it doesn't belong on that day. */
  let proposedOnPreviewedDay = $derived.by(() => {
    const start = new Date(invite.start).getTime()
    const end = new Date(invite.end).getTime()
    return (
      end > previewDayStart.getTime() && start < previewDayEnd.getTime()
    )
  })

  /** Localised "Wed, Apr 29" label for the day currently
   *  shown in the preview — drives the centre label between
   *  the prev/next day arrows. */
  let previewDayLabel = $derived(
    previewDate.toLocaleDateString(undefined, {
      weekday: 'short',
      month: 'short',
      day: 'numeric',
    }),
  )

  // Auto-scroll the preview to the proposed slot the first time
  // the user opens the details panel, then once again whenever
  // they navigate to a different invite (different uid).  Avoids
  // both (a) opening on the 00:00–08:00 dead zone and (b)
  // re-snapping if the user manually scrolls within the panel.
  let previewScrollEl: HTMLDivElement | undefined = $state()
  // Track the (uid, day) pair we last auto-scrolled for so
  // arrow-day-navigation re-centers the viewport on the new
  // day's "interesting hours" (proposed slot if present, else
  // 08:00 as a sensible default), but doesn't fight the user
  // if they manually scroll within a day.
  let lastScrollKey = $state<string | null>(null)
  $effect(() => {
    if (!detailsOpen) {
      lastScrollKey = null
      return
    }
    if (!previewScrollEl) return
    const key = `${invite.uid}|${dayKey(previewDate)}`
    if (lastScrollKey === key) return
    let targetMin: number
    if (proposedOnPreviewedDay) {
      targetMin = Math.max(
        0,
        minutesFromDayStart(invite.start, previewDayStart),
      )
    } else {
      targetMin = 8 * 60
    }
    const target = Math.max(0, (targetMin / 60) * HOUR_PX - HOUR_PX)
    previewScrollEl.scrollTop = target
    lastScrollKey = key
  })

  /** Display name preference for an attendee chip: "You" when
   *  the row matches one of the user's configured mail account
   *  identities (the address the invite landed on), CN if
   *  given, else the local-part of the email so a list of bare
   *  emails doesn't fill the panel with redundant "@domain"
   *  suffixes. */
  function attendeeName(a: InviteAttendee): string {
    if (userIdentities.has(a.email.toLowerCase())) return 'You'
    if (a.common_name && a.common_name.trim()) return a.common_name.trim()
    const at = a.email.indexOf('@')
    return at > 0 ? a.email.slice(0, at) : a.email
  }

  /** Two-letter avatar fallback for the attendee chips — same
   *  pattern EventEditor's chips use so the visual rhythm
   *  carries across surfaces. */
  function attendeeInitials(a: InviteAttendee): string {
    const src = attendeeName(a).trim()
    if (!src) return '?'
    const parts = src.split(/\s+/).filter(Boolean)
    if (parts.length === 1) return parts[0][0].toUpperCase()
    return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase()
  }

  // ── Contact-photo lookup for attendee chips (#179) ──────────
  // Mirrors EventEditor's pattern: warm a small per-card cache
  // keyed by lowercase email, then resolve `<img src>` against
  // the `contact-photo://` scheme.  Falls back to initials when
  // the address has no vCard photo.
  interface ContactEmail {
    kind: string
    value: string
  }
  interface ContactRow {
    id: string
    display_name: string
    email: ContactEmail[]
    organization: string | null
    photo_mime: string | null
  }
  let contactsByEmail = $state<Map<string, ContactRow>>(new Map())
  $effect(() => {
    void invoke<ContactRow[]>('search_contacts', { query: '', limit: 500 })
      .then((rows) => {
        const map = new Map<string, ContactRow>()
        for (const c of rows) {
          for (const e of c.email) {
            if (e.value) map.set(e.value.toLowerCase(), c)
          }
        }
        contactsByEmail = map
      })
      .catch(() => {})
  })
  function attendeePhotoUrl(a: InviteAttendee): string | null {
    const c = contactsByEmail.get(a.email.toLowerCase())
    if (!c || !c.photo_mime) return null
    return convertFileSrc(c.id, 'contact-photo')
  }

  /** Optimistic PARTSTAT overrides keyed by lower-cased email,
   *  applied on top of the static `invite.attendees` snapshot
   *  so the user's chip flips to their just-picked response
   *  the moment the RSVP IPC succeeds — without waiting for a
   *  CalDAV round-trip + resync to land a fresh `invite` prop. */
  let attendeeStatusOverrides = $state<Map<string, string>>(new Map())
  function effectiveStatus(a: InviteAttendee): string {
    // 1) In-session override — set when the user clicks Accept /
    //    Tentative / Decline.
    const override = attendeeStatusOverrides.get(a.email.toLowerCase())
    if (override) return override.toUpperCase()
    // 2) Persisted PARTSTAT from `respondedAs`, which the mount
    //    effect hydrates from the cached calendar event (and the
    //    fallback `rsvp_responses` table).  This keeps the user's
    //    own chip stamped with their answer when they reopen the
    //    invite later — without it, the chip would reset to the
    //    static `invite.attendees` PARTSTAT (typically NEEDS-
    //    ACTION on the original REQUEST body).
    if (respondedAs && userIdentities.has(a.email.toLowerCase())) {
      return respondedAs
    }
    return (a.status ?? 'NEEDS-ACTION').toUpperCase()
  }
  /** `HH:MM` from an ISO timestamp, in local time — matches the
   *  display the rest of the card uses for `timeRange`. */
  function fmtClock(iso: string): string {
    return new Date(iso).toLocaleTimeString(undefined, {
      hour: '2-digit',
      minute: '2-digit',
    })
  }
  let proposedRangeLabel = $derived(`${fmtClock(invite.start)}–${fmtClock(invite.end)}`)

  /** Build an inline style string for an existing event block
   *  using the calendar's identity colour, mirroring the
   *  agenda grid's `.ev-block` palette so the previews share
   *  visual language with the main calendar.  Inline (not
   *  scoped CSS) to keep the styling immune to compound-class
   *  scoping edge cases. */
  function existingEventStyle(colour: string, status: string | null): string {
    const c = colour
    const s = (status ?? '').toUpperCase()
    if (s === 'DECLINED') {
      return [
        `background: transparent`,
        `border: 1.5px solid ${c}`,
        `color: ${c}`,
        `text-decoration: line-through`,
        `opacity: 0.85`,
      ].join('; ')
    }
    const base = [
      `background-color: color-mix(in srgb, ${c} 22%, transparent)`,
      `border: 1px solid color-mix(in srgb, ${c} 45%, transparent)`,
      `box-shadow: inset 3px 0 0 0 ${c}`,
      `color: ${c}`,
      `padding-left: 8px`,
    ]
    if (s === 'TENTATIVE') {
      base.push(
        `background-image: repeating-linear-gradient(45deg, transparent 0, transparent 6px, color-mix(in srgb, ${c} 35%, transparent) 6px, color-mix(in srgb, ${c} 35%, transparent) 8px)`,
      )
    }
    return base.join('; ')
  }

  /** Recover the bare VEVENT UID from a composite calendar
   *  event id (`{nc_id}::{cal_path}::{uid}` or
   *  `{…}::{uid}::occ::{epoch}`).  Used to detect whether the
   *  proposed invite already lives on one of the user's
   *  calendars. */
  function eventUid(id: string): string {
    const parts = id.split('::')
    return parts[2] ?? ''
  }
  /** When the proposed event is *already* on a visible
   *  calendar (e.g. the user accepted earlier), pull that row
   *  out of the preview list so we can render the proposed
   *  slot in its real calendar colour + PARTSTAT visual
   *  instead of the generic "primary outline" placeholder. */
  let matchedExistingEvent = $derived(
    previewEvents.find((ev) => eventUid(ev.id) === invite.uid) ?? null,
  )
  let proposedExistsInCalendar = $derived(matchedExistingEvent !== null)
  let proposedCalendarColor = $derived(
    matchedExistingEvent
      ? (calendarsById.get(eventCalendarId(matchedExistingEvent.id))?.color ?? '#2bb0ed')
      : null,
  )

  /** Stroke-icon + colour token pair for an attendee's PARTSTAT.
   *  Matches the alphabet the RSVP dropdown in EventEditor uses
   *  so the visual language carries across the app — and lets
   *  callers drop the `<Icon>` inside a `class="text-…"` wrapper
   *  to colour it without having to repeat the mapping. */
  function attendeeStatusGlyph(a: InviteAttendee): { name: IconName; colorClass: string } {
    const s = effectiveStatus(a)
    if (s === 'ACCEPTED') return { name: 'rsvp-accept', colorClass: 'text-success-500' }
    if (s === 'DECLINED') return { name: 'rsvp-decline', colorClass: 'text-error-500' }
    if (s === 'TENTATIVE') return { name: 'rsvp-tentative', colorClass: 'text-warning-500' }
    return { name: 'help', colorClass: 'text-surface-500' }
  }

  // Fetch calendars + the user's default-calendar setting on
  // mount.  Both go through Tauri.  We don't gate the card on
  // these completing (the user can read the invite while the
  // picker spins up), but we do disable the buttons until the
  // selection is established.
  $effect(() => {
    calendarsLoading = true
    void Promise.all([
      invoke<{ default_calendar_id: string | null }>('get_app_settings'),
      (async () => {
        const accounts = await invoke<{ id: string }[]>('get_nextcloud_accounts')
        const all: CalendarRow[] = []
        for (const acc of accounts) {
          try {
            const cs = await invoke<CalendarRow[]>('get_cached_calendars', {
              ncId: acc.id,
            })
            all.push(...cs)
          } catch (e) {
            console.warn('CalendarInviteCard: get_cached_calendars failed', e)
          }
        }
        return all.filter((c) => !c.hidden)
      })(),
      (async () => {
        // Match the invite's ATTENDEE list against the user's
        // configured mail accounts so the backend has a verified
        // address-that-is-actually-in-the-invite to use as the
        // mutation target.  Skips silently when the lookup
        // fails — backend has its own fallbacks.
        try {
          const list = await invoke<{ email: string }[]>('get_accounts')
          const owned = new Set(
            list.map((a) => a.email.toLowerCase()).filter(Boolean),
          )
          return (
            invite.attendees
              .map((a) => a.email)
              .find((addr) => owned.has(addr.toLowerCase())) ?? null
          )
        } catch {
          return null
        }
      })(),
    ])
      .then(([settings, list, hint]) => {
        calendars = list
        attendeeHint = hint
        const def = settings.default_calendar_id
        if (def && list.some((c) => c.id === def)) {
          selectedCalendarId = def
        } else if (list.length > 0) {
          selectedCalendarId = list[0].id
        }
      })
      .catch((e) => {
        console.warn('CalendarInviteCard: failed to load calendars', e)
      })
      .finally(() => {
        calendarsLoading = false
      })
  })

  type Partstat = 'ACCEPTED' | 'DECLINED' | 'TENTATIVE'
  let busy = $state<Partstat | null>(null)
  let respondedAs = $state<Partstat | null>(null)
  let error = $state('')
  /** True until the partstat lookup for the *current* invite
   *  has completed at least once.  The action row is suppressed
   *  while this is true so the user never sees the fresh
   *  Accept/Decline buttons flash and snap to the post-reply
   *  state — the row appears in its final shape directly.
   *
   *  Tracked per-UID so navigating to a different invite
   *  re-gates rendering, but in-place reactivity (e.g.
   *  `attendeeHint` arriving from a parallel effect) does NOT
   *  re-trigger the gate: the previously-resolved state stays
   *  visible while we re-query in the background, no flicker. */
  let partstatLoadedUid = $state<string | null>(null)
  /** Tracks completion of the parallel cancellation probe
   *  (`is_invite_cancelled` + `is_event_in_calendar`).  Paired
   *  with `partstatLoadedUid` to gate the *entire card* — not
   *  just the action row.  Without that broader gate the
   *  outer flavour (blue REQUEST border vs red CANCEL border,
   *  the title strikethrough, the leading icon) paints in
   *  REQUEST mode for a moment and then snaps to CANCEL once
   *  `cancelledLater` lands, which the user reads as a
   *  flicker.  Holding the card until both probes are in lets
   *  it appear in its final flavour from the first paint. */
  let cancellationProbedUid = $state<string | null>(null)
  let cardReady = $derived(
    !!invite.uid &&
      partstatLoadedUid === invite.uid &&
      cancellationProbedUid === invite.uid,
  )
  /** Set when MailView has previously seen a `METHOD:CANCEL`
   *  mail for this UID — flips the card to the cancelled
   *  flavour even when the user is viewing the original
   *  REQUEST.  Probed once on mount via `is_invite_cancelled`;
   *  null until the lookup resolves (treated as "not cancelled
   *  yet" — the card renders the regular RSVP UI in that
   *  window so we never *hide* the buttons by mistake while
   *  waiting). */
  let cancelledLater = $state(false)
  // Card flavour:
  //   - REQUEST → Accept / Tentative / Decline RSVP UI (unless
  //     post-hoc cancellation has been recorded for the UID,
  //     in which case it flips to the cancelled banner so the
  //     user can't unwittingly answer a cancelled meeting).
  //   - CANCEL  → "Remove from my calendar" affordance.
  let isCancel = $derived(
    (invite.method ?? '').toUpperCase() === 'CANCEL' || cancelledLater,
  )
  let dismissingCancel = $state(false)
  let dismissed = $state(false)
  /** Whether a calendar entry for this UID currently exists
   *  locally — drives the CANCEL flavour's affordance.  Probed
   *  on mount via `is_event_in_calendar`; null until the lookup
   *  resolves.  When the event isn't in any cached calendar
   *  (user never accepted, or already removed it), we hide the
   *  "Remove from my calendar" button and show a passive line
   *  instead — there's nothing to remove. */
  let eventInCalendar = $state<boolean | null>(null)
  $effect(() => {
    const uid = invite.uid
    // Same logic as the partstat effect: only reset on UID
    // changes, never on incidental re-fires — so the resolved
    // state stays visible while we re-probe.
    if (cancellationProbedUid !== uid) {
      eventInCalendar = null
      cancelledLater = false
    }
    if (!uid) return
    void Promise.all([
      invoke<boolean>('is_event_in_calendar', { uid }),
      invoke<boolean>('is_invite_cancelled', { uid }),
    ])
      .then(([present, cancelled]) => {
        if (invite.uid !== uid) return
        eventInCalendar = present
        cancelledLater = cancelled
      })
      .catch((e) => {
        console.warn('cancellation/in-calendar probe failed', e)
      })
      .finally(() => {
        if (invite.uid === uid) cancellationProbedUid = uid
      })
  })
  async function dismissCancelledEvent() {
    if (dismissingCancel || dismissed) return
    error = ''
    dismissingCancel = true
    try {
      await invoke('dismiss_cancelled_event', { uid: invite.uid })
      dismissed = true
    } catch (e) {
      error = formatError(e) || 'Failed to remove the cancelled event'
    } finally {
      dismissingCancel = false
    }
  }

  // Re-hydrate the post-reply state whenever the invite changes
  // (the user clicked through to a different mail).  Source of
  // truth = the user's ATTENDEE PARTSTAT on the cached calendar
  // event (synced from CalDAV), which captures changes made via
  // NC web UI / phone / any other client too.  Falls back to the
  // local `rsvp_responses` table when the event isn't in the
  // calendar cache yet (e.g. first-time render before the
  // background sync completes).  Keyed by UID since a single
  // invite can show up in multiple folders / accounts and
  // should agree everywhere.
  // Hydrate the post-reply state whenever the open invite
  // changes.  Two sources of truth, in priority order:
  //   1. The cached calendar event's user-PARTSTAT (after a
  //      CalDAV delta sync).  Authoritative server-side state
  //      — captures changes made via NC web UI / phone / any
  //      other CalDAV client.
  //   2. The local `rsvp_responses` table — last-resort
  //      fallback used only when (1) returns null (the event
  //      isn't on any calendar any more — common after a CANCEL
  //      flow or after the user manually removed the event).
  //      Without this, the card would forget the user's
  //      previous response the moment the calendar entry is
  //      gone.  We only trust it when the cached event is
  //      genuinely missing — never when it's present with a
  //      different PARTSTAT — so stale rsvp_responses entries
  //      don't override authoritative server state.
  // Render is NOT gated on either lookup resolving:
  // `respondedAs` starts null so fresh Accept/Decline buttons
  // render immediately and only get replaced when a valid
  // prior PARTSTAT comes back.
  $effect(() => {
    const uid = invite.uid
    // Only reset the post-reply state when the user actually
    // navigated to a different invite — not on incidental
    // reactive re-fires (e.g. `attendeeHint` arriving later).
    // That keeps the resolved state visible while a background
    // re-query runs, and prevents the "fresh buttons flash and
    // snap" the user complained about.
    if (partstatLoadedUid !== uid) {
      respondedAs = null
      error = ''
    }
    if (!uid) return
    void (async () => {
      try {
        const valid = (s: string | null): s is Partstat =>
          s === 'ACCEPTED' || s === 'DECLINED' || s === 'TENTATIVE'
        const partstat = await invoke<string | null>(
          'get_event_partstat_for_user',
          { uid, attendeeHint: attendeeHint },
        )
        if (invite.uid !== uid) return
        if (valid(partstat)) {
          respondedAs = partstat
        } else {
          // No PARTSTAT on a calendar event — try the local
          // persistence table.
          const local = await invoke<string | null>('get_rsvp_response', { uid })
          if (invite.uid !== uid) return
          respondedAs = valid(local) ? local : null
        }
      } catch (e) {
        console.warn('partstat hydration failed', e)
      } finally {
        if (invite.uid === uid) partstatLoadedUid = uid
      }
    })()
  })

  /** Human-readable past-tense verb for the chosen response.
   *  Used both in the "You replied: …" callout and as the label
   *  on the highlighted button after the user has answered. */
  function verbFor(p: Partstat): string {
    if (p === 'ACCEPTED') return 'Accepted'
    if (p === 'DECLINED') return 'Declined'
    return 'Tentative'
  }

  /** Pre-reply (or "change to") imperative label for each option. */
  function actionLabel(p: Partstat): string {
    if (p === 'ACCEPTED') return 'Accept'
    if (p === 'DECLINED') return 'Decline'
    return 'Tentative'
  }

  /** Same RSVP icon family the chip render uses, indexed by
   *  PARTSTAT.  Pulled out so the post-reply "Change to …"
   *  buttons share the visual language with everything else. */
  function partstatGlyph(p: Partstat): { name: IconName; colorClass: string } {
    if (p === 'ACCEPTED') return { name: 'rsvp-accept', colorClass: 'text-success-500' }
    if (p === 'DECLINED') return { name: 'rsvp-decline', colorClass: 'text-error-500' }
    return { name: 'rsvp-tentative', colorClass: 'text-warning-500' }
  }

  /** Format the meeting slot the user is being asked to commit to.
   *  Same shape as the body block Compose injects (sameDay → one
   *  date, otherwise two full timestamps) so the visual language
   *  stays consistent across "I'm sending one" and "I got one". */
  let timeRange = $derived.by(() => {
    const start = new Date(invite.start)
    const end = new Date(invite.end)
    const sameDay =
      start.getFullYear() === end.getFullYear() &&
      start.getMonth() === end.getMonth() &&
      start.getDate() === end.getDate()
    const dateStr = start.toLocaleDateString(undefined, {
      weekday: 'short',
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    })
    const timeFmt: Intl.DateTimeFormatOptions = { hour: '2-digit', minute: '2-digit' }
    // Multi-day events: build the format from explicit fields,
    // not `dateStyle` — `Intl.DateTimeFormat` rejects mixing
    // `dateStyle`/`timeStyle` with field-level options like
    // `hour`/`minute` (`TypeError`).  Fields-only is the safe
    // shape and matches the same-day branch's resolution.
    const dateTimeFmt: Intl.DateTimeFormatOptions = {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    }
    return sameDay
      ? `${dateStr} · ${start.toLocaleTimeString(undefined, timeFmt)} – ${end.toLocaleTimeString(undefined, timeFmt)}`
      : `${start.toLocaleString(undefined, dateTimeFmt)} – ${end.toLocaleString(undefined, dateTimeFmt)}`
  })

  /** Approximate duration string ("1h 30m") for the time slot —
   *  helps the user decide quickly whether they can fit the
   *  meeting in. */
  let durationLabel = $derived.by(() => {
    const ms = new Date(invite.end).getTime() - new Date(invite.start).getTime()
    if (!isFinite(ms) || ms <= 0) return ''
    const totalMin = Math.round(ms / 60_000)
    const hours = Math.floor(totalMin / 60)
    const mins = totalMin % 60
    if (hours === 0) return `${mins}m`
    if (mins === 0) return `${hours}h`
    return `${hours}h ${mins}m`
  })

  async function rsvp(partstat: Partstat) {
    // Allow clicking the same response again as a no-op, but
    // skip while a request's already in flight so a double-click
    // doesn't double-PUT.
    if (busy) return
    if (respondedAs === partstat) return
    if (!selectedCalendarId) {
      error = 'Pick a calendar before responding.'
      return
    }
    error = ''
    busy = partstat
    try {
      // The backend writes the user's PARTSTAT into the chosen
      // calendar via CalDAV.  Nextcloud's iMIP plugin (NC 30+
      // Mail Provider) handles the REPLY mail to the organiser
      // automatically — the client never touches SMTP for RSVPs.
      // For DECLINED, the backend PUT-then-DELETEs so the
      // organiser is notified but the entry doesn't clutter the
      // user's calendar.
      await invoke('respond_to_invite', {
        calendarId: selectedCalendarId,
        rawIcs: invite.rawIcs,
        partstat,
        attendeeHint,
      })
      respondedAs = partstat
      onresponded?.(partstat)
      // Optimistically flip the user's row in the attendee chip
      // strip so the emoji reflects the just-picked response
      // without waiting for a fresh `invite` prop to land.
      // Keyed by the resolved attendee email if we have one, else
      // every userIdentities entry (covers aliases / address
      // variants so the right row matches whichever address NC
      // has on the ATTENDEE line).
      const next = new Map(attendeeStatusOverrides)
      const targets = attendeeHint
        ? [attendeeHint.toLowerCase()]
        : Array.from(userIdentities)
      for (const t of targets) next.set(t, partstat)
      attendeeStatusOverrides = next
      // Drop the per-day events cache so the day preview re-fetches
      // and immediately reflects the just-recorded RSVP — the
      // proposed slot's "is this on my calendar?" detection and
      // the matched event's PARTSTAT visual both depend on
      // `previewEvents`.  Reset the auto-scroll key so the
      // viewport snaps back to the proposed slot like a fresh
      // open (otherwise the user would have to scroll manually
      // to see the new "✉ invite" treatment land).
      previewEventsByDate = new Map()
      lastScrollKey = null
      if (detailsOpen) {
        void loadPreviewForDate(previewDate)
      }
    } catch (e) {
      error = formatError(e) || 'Failed to record RSVP'
    } finally {
      busy = null
    }
  }
</script>

{#if cardReady}
<div class="rounded-md p-4 mb-3 text-sm
            {isCancel
              ? 'border border-red-500/40 bg-red-500/5'
              : 'border border-primary-500/40 bg-primary-500/5'}">
  <div class="flex items-start justify-between gap-3 mb-2">
    <div class="flex items-center gap-2">
      <span class={isCancel ? 'text-error-500' : 'text-primary-500'}>
        <Icon name={isCancel ? 'rsvp-canceled' : 'calendar'} size={18} />
      </span>
      <span class="font-semibold {isCancel ? 'line-through' : ''}">
        {invite.summary || '(untitled meeting)'}
      </span>
      {#if isCancel}
        <span class="text-[10px] uppercase tracking-wide font-semibold px-1.5 py-px rounded bg-red-500 text-white">
          Cancelled
        </span>
      {/if}
    </div>
  </div>

  <!-- Time slot.  Front-and-centre so the user can decide
       "does this fit" before clicking anything. -->
  <div class="text-surface-700 dark:text-surface-300 mb-1 flex items-center gap-1.5">
    <Icon name="time" size={14} class="shrink-0" />
    <span class="font-medium">{timeRange}</span>
    {#if durationLabel}
      <span class="text-surface-500"> · {durationLabel}</span>
    {/if}
  </div>

  {#if invite.location}
    <div class="text-surface-700 dark:text-surface-300 mb-1 flex items-center gap-1.5">
      <Icon name="location" size={14} class="shrink-0" />
      <span>{invite.location}</span>
    </div>
  {/if}
  {#if invite.url}
    <div class="text-surface-700 dark:text-surface-300 mb-1 truncate">
      🔗 <a class="text-primary-500 hover:underline" href={invite.url}>{invite.url}</a>
    </div>
  {/if}
  {#if invite.attendees.length > 0}
    <div class="text-xs text-surface-500 mt-1 flex items-center gap-1.5">
      <Icon name="contacts" size={12} class="shrink-0" />
      <span>{invite.attendees.length} attendee{invite.attendees.length === 1 ? '' : 's'}</span>
    </div>
  {/if}

  <!-- Calendar picker.  Defaults to the user's "default
       calendar" app setting; falls back to the first non-hidden
       calendar across all connected Nextcloud accounts.  Hidden
       once the user has answered (the event is now in the
       chosen calendar — moving it elsewhere would need a
       separate UI), and entirely suppressed for CANCEL flavour
       (no calendar is being added to). -->
  {#if !isCancel && !respondedAs && calendars.length > 1}
    <div class="flex items-center gap-2 mt-3 text-xs">
      <label class="text-surface-500" for="rsvp-calendar">Add to</label>
      <select
        id="rsvp-calendar"
        class="select px-2 py-1 text-xs rounded-md flex-1 max-w-[260px]"
        bind:value={selectedCalendarId}
        disabled={calendarsLoading}
      >
        {#each calendars as c (c.id)}
          <option value={c.id}>{c.display_name}</option>
        {/each}
      </select>
    </div>
  {/if}

  {#if error}
    <p class="text-xs text-red-500 mt-2">{error}</p>
  {/if}

  <!-- RSVP action area — placed above the "More info" /
       day-preview block so the answer affordance stays at the
       top of the card and the user can reply without scrolling
       past the (optional, expandable) details panel. -->
  {#if isCancel}
    <!-- CANCEL flavour.  The organiser dropped the meeting; we
         offer a single button to remove the local copy.  Once
         dismissed (or if the event isn't in the user's calendar
         to begin with), the button collapses to a confirmation
         line so the card communicates "you're done here". -->
    <p class="text-sm text-surface-700 dark:text-surface-300 mt-3">
      The organiser cancelled this meeting.
    </p>
    <div class="flex flex-wrap items-center gap-2 mt-2">
      {#if dismissed}
        <span class="text-xs text-surface-500 italic">
          ✓ Removed from your calendar
        </span>
      {:else if eventInCalendar === false}
        <!-- The meeting isn't on any of the user's calendars
             — nothing to remove.  Show a passive line so the
             card still communicates state without surfacing a
             button that would no-op. -->
        <span class="text-xs text-surface-500 italic">
          Not in your calendar — nothing to remove.
        </span>
      {:else}
        <button
          class="btn btn-sm preset-filled-error-500 disabled:opacity-50"
          disabled={dismissingCancel || eventInCalendar === null}
          onclick={() => void dismissCancelledEvent()}
        >
          {#if dismissingCancel}
            Removing…
          {:else}
            <Icon name="trash" size={14} class="inline-block align-text-bottom mr-1.5" />Remove from my calendar
          {/if}
        </button>
      {/if}
    </div>
  {:else if respondedAs}
    <!-- Post-reply state.  Chosen option = past-tense label
         ("Accepted") with a primary-coloured *border* (no fill)
         to distinguish it from the unanswered "click to commit"
         pre-reply state.  Alternatives become outlined "Change
         to …" so the user can flip their mind without losing
         the visual confirmation of "what did I answer?". -->
    <p class="text-sm text-surface-700 dark:text-surface-300 mt-3">
      <span class="font-medium">You replied:</span> {verbFor(respondedAs)}
    </p>
    <div class="flex flex-wrap items-center gap-2 mt-2">
      {#each (['ACCEPTED', 'TENTATIVE', 'DECLINED'] as Partstat[]) as p}
        {@const isCurrent = respondedAs === p}
        {@const g = partstatGlyph(p)}
        <button
          class="btn btn-sm disabled:opacity-50 inline-flex items-center gap-1.5 {isCurrent
            ? 'border-2 border-primary-500 text-primary-500 bg-transparent hover:bg-primary-500/5 font-semibold'
            : 'preset-outlined-surface-500'}"
          disabled={busy !== null}
          onclick={() => void rsvp(p)}
          title={isCurrent ? 'Your current response' : `Change response to ${verbFor(p)}`}
        >
          {#if busy === p}
            Sending…
          {:else}
            <span class={g.colorClass}><Icon name={g.name} size={14} /></span>
            {isCurrent ? verbFor(p) : `Change to ${actionLabel(p)}`}
          {/if}
        </button>
      {/each}
    </div>
  {:else}
    <div class="flex flex-wrap items-center gap-2 mt-3">
      <button
        class="btn btn-sm preset-filled-primary-500 disabled:opacity-50 inline-flex items-center gap-1.5"
        disabled={busy !== null}
        onclick={() => void rsvp('ACCEPTED')}
      >
        {#if busy === 'ACCEPTED'}
          Sending…
        {:else}
          <Icon name="rsvp-accept" size={14} /> Accept
        {/if}
      </button>
      <button
        class="btn btn-sm preset-outlined-surface-500 disabled:opacity-50 inline-flex items-center gap-1.5"
        disabled={busy !== null}
        onclick={() => void rsvp('TENTATIVE')}
      >
        {#if busy === 'TENTATIVE'}
          Sending…
        {:else}
          <span class="text-warning-500"><Icon name="rsvp-tentative" size={14} /></span> Tentative
        {/if}
      </button>
      <button
        class="btn btn-sm preset-outlined-surface-500 disabled:opacity-50 inline-flex items-center gap-1.5"
        disabled={busy !== null}
        onclick={() => void rsvp('DECLINED')}
      >
        {#if busy === 'DECLINED'}
          Sending…
        {:else}
          <span class="text-error-500"><Icon name="rsvp-decline" size={14} /></span> Decline
        {/if}
      </button>
    </div>
  {/if}

  <!-- "More info" toggle.  Cheap to render closed — the
       attendee chip list and the day-preview grid only mount
       when the user opens the panel, and the events for the
       grid are fetched lazily on first expand. -->
  <button
    type="button"
    class="mt-2 inline-flex items-center gap-1 text-xs text-primary-600 dark:text-primary-300 hover:text-primary-700 dark:hover:text-primary-200"
    onclick={toggleDetails}
    aria-expanded={detailsOpen}
  >
    <span class="transition-transform inline-flex {detailsOpen ? 'rotate-90' : ''}"><Icon name="nav-forward" size={12} /></span>
    {detailsOpen ? 'Hide details' : 'More info'}
  </button>

  {#if detailsOpen}
    <div class="mt-3 pt-3 border-t border-surface-300/40 dark:border-surface-600/40 space-y-3">
      <!-- Attendee chips — read-only mirror of EventEditor's
           chip list, but stripped of the photo + edit affordances
           so the panel stays compact.  Status dot encodes the
           PARTSTAT at a glance: green=accepted, amber=tentative,
           red=declined, grey=no response. -->
      {#if invite.attendees.length > 0}
        <div>
          <div class="text-[10px] uppercase tracking-wider text-surface-500 mb-1.5">
            Attendees ({invite.attendees.length})
          </div>
          <!-- Attendee chips — same shape EventEditor uses
               (avatar + RSVP overlay badge), just simpler
               because the invite card has no contact-photo
               lookup so every avatar shows initials. -->
          <div class="flex flex-wrap gap-2">
            {#each invite.attendees as a (a.email)}
              {@const partstat = effectiveStatus(a).toUpperCase()}
              {@const showRsvpBadge = partstat && partstat !== 'NEEDS-ACTION'}
              {@const g = attendeeStatusGlyph(a)}
              {@const photo = attendeePhotoUrl(a)}
              <span
                class="inline-flex items-center gap-3 pl-1 pr-2 py-1 rounded-full text-xs bg-surface-200 dark:bg-surface-700 max-w-full"
                title={a.email + ` — ${partstat.toLowerCase()}`}
              >
                <span class="relative w-7 h-7 flex-shrink-0">
                  {#if photo}
                    <img
                      src={photo}
                      alt=""
                      loading="lazy"
                      class="w-7 h-7 rounded-full object-cover"
                    />
                  {:else}
                    <div class="w-7 h-7 rounded-full bg-surface-300 dark:bg-surface-600 flex items-center justify-center text-[11px] font-semibold">
                      {attendeeInitials(a)}
                    </div>
                  {/if}
                  {#if showRsvpBadge}
                    <span
                      class="absolute bottom-0 -right-2 w-4 h-4 rounded-full bg-surface-50 dark:bg-surface-900 ring-1 ring-surface-200 dark:ring-surface-700 flex items-center justify-center leading-none {g.colorClass}"
                      aria-label={`Response: ${partstat.toLowerCase()}`}
                    >
                      <Icon name={g.name} size={10} class="block" />
                    </span>
                  {/if}
                </span>
                <span class="truncate max-w-[180px] leading-tight font-medium">{attendeeName(a)}</span>
              </span>
            {/each}
          </div>
        </div>
      {/if}

      <!-- Day preview grid.  Full 24h axis with even hour-row
           spacing (CSS grid), scrollable, with prev/next-day
           arrows around a centred date label.  Events render on
           top via an absolutely-positioned overlay so they can
           span arbitrary minutes inside an hour cell. -->
      <div>
        <div class="text-[10px] uppercase tracking-wider text-surface-500 mb-1.5">Your day</div>
        <!-- Day navigation: ◀ <date> ▶ — keeps the date as the
             visual anchor between the arrows so the user reads
             a clear "where am I" line at a glance. -->
        <div class="flex items-center justify-center gap-2 mb-2 text-xs">
          <button
            type="button"
            class="w-7 h-7 rounded-md flex items-center justify-center text-surface-600 dark:text-surface-300 hover:bg-surface-200 dark:hover:bg-surface-700 transition-colors"
            aria-label="Previous day"
            title="Previous day"
            onclick={() => shiftPreviewDay(-1)}
          ><Icon name="nav-backward" size={14} /></button>
          <span class="font-medium text-surface-700 dark:text-surface-200 min-w-[120px] text-center">
            {previewDayLabel}
          </span>
          <button
            type="button"
            class="w-7 h-7 rounded-md flex items-center justify-center text-surface-600 dark:text-surface-300 hover:bg-surface-200 dark:hover:bg-surface-700 transition-colors"
            aria-label="Next day"
            title="Next day"
            onclick={() => shiftPreviewDay(1)}
          ><Icon name="nav-forward" size={14} /></button>
        </div>

        {#if previewLoading && !previewLoaded}
          <p class="text-xs text-surface-500 italic">Loading…</p>
        {:else if previewError}
          <p class="text-xs text-red-500">{previewError}</p>
        {:else}
          {@const proposedGeom = proposedOnPreviewedDay ? blockGeometry(invite.start, invite.end) : null}
          <div
            bind:this={previewScrollEl}
            class="relative rounded-md border border-surface-200 dark:border-surface-700 bg-surface-50/60 dark:bg-surface-900/40 overflow-y-auto"
            style="max-height: {PREVIEW_VIEWPORT_PX}px;"
          >
            <!-- Hour rows: 24 fixed-height grid rows guarantee
                 perfectly even spacing regardless of how the
                 absolute event overlay paints on top of them. -->
            <div
              class="relative grid"
              style="grid-template-rows: repeat(24, {HOUR_PX}px); height: {PREVIEW_TOTAL_PX}px;"
            >
              {#each Array.from({ length: 24 }, (_, i) => i) as h (h)}
                <div class="border-t border-surface-200/70 dark:border-surface-700/70 relative">
                  <span class="absolute left-1 top-0.5 text-[9px] text-surface-400 leading-none">
                    {String(h).padStart(2, '0')}:00
                  </span>
                </div>
              {/each}

              <!-- Event overlay layer: absolute children
                   positioned against the grid container above. -->
              <div class="absolute inset-0 pointer-events-none">
                <!-- Existing events on the left half.  Each
                     block carries the start–end time inline in
                     the top-right corner if it's tall enough
                     (≥28px), and always exposes the times via
                     the hover tooltip. -->
                {#each previewEvents as ev (ev.id)}
                  {#if eventUid(ev.id) !== invite.uid}
                    {@const g = blockGeometry(ev.start, ev.end)}
                    {#if g}
                      {@const cal = calendarsById.get(eventCalendarId(ev.id))}
                      {@const colour = cal?.color ?? '#2bb0ed'}
                      {@const range = `${fmtClock(ev.start)}–${fmtClock(ev.end)}`}
                      {@const showInline = g.height >= 28}
                      {@const partstat = userPartstatOn(ev)}
                      <div
                        class="absolute rounded-sm text-[10px] overflow-hidden pointer-events-auto"
                        style="left: 36px; right: 4px; top: {g.top}px; height: {g.height}px; {existingEventStyle(colour, partstat)}"
                        title={`${ev.summary || '(no title)'} — ${range}${cal ? ` · ${cal.display_name}` : ''}${partstat && partstat !== 'ACCEPTED' ? ` (${partstat.toLowerCase()})` : ''}`}
                      >
                        {#if showInline}
                          <div class="truncate font-medium leading-tight pr-10">{ev.summary || '(no title)'}</div>
                          <span
                            class="absolute top-0.5 right-1 text-[9px] font-mono tabular-nums leading-none text-surface-500 dark:text-surface-400"
                          >{range}</span>
                        {:else}
                          <div class="truncate leading-tight">{ev.summary || '(no title)'}</div>
                        {/if}
                      </div>
                    {/if}
                  {/if}
                {/each}

                <!-- Proposed event sits in the same column as the
                     existing events (#236) — overlaid on top so
                     the user can read it against the surrounding
                     calendar in context.  Inline styles only — no
                     compound scoped classes — so the box is
                     guaranteed to render with a visible border +
                     fill regardless of CSS scoping quirks.  Two
                     flavours:
                     - already-on-calendar → calendar-coloured
                       fill (or stripes / declined-border per
                       the user's PARTSTAT) plus the primary
                       ring-2 + invite pill so the user can tell
                       which event the invite refers to.  The
                       same-UID existing event is filtered out of
                       the preview loop above so this is the
                       only render of that event, no overlap.
                     - not-on-calendar → primary dashed border
                       + light primary tint to signal "what you
                       would be adding".  When this overlaps a
                       real existing event, the lighter tint lets
                       the existing event's coloured strip show
                       through behind the dashed outline. -->
                {#if proposedGeom}
                  {@const tooltip = proposedExistsInCalendar
                    ? `${invite.summary || '(untitled)'} — from this invite (already on your calendar, ${respondedAs ? respondedAs.toLowerCase() : 'accepted'})`
                    : `${invite.summary || '(untitled)'} — proposed`}
                  {@const showInlineProp = proposedGeom.height >= 28}
                  {#if proposedExistsInCalendar}
                    {@const c = proposedCalendarColor ?? '#2bb0ed'}
                    <!-- Existing-on-calendar branch: render in
                         the matched calendar's identity colour
                         (transparent fill + bordered + accent
                         bar) and stamp an "✉ Invite" pill in the
                         top-right corner so the user can
                         immediately tell *this is the meeting
                         the email is about*, distinguishing it
                         from any neighbouring event in the same
                         calendar. -->
                    <div
                      class="absolute rounded-sm text-[10px] font-medium overflow-hidden pointer-events-auto ring-2 ring-primary-500/70"
                      style="left: 36px; right: 4px; top: {proposedGeom.top}px; height: {proposedGeom.height}px; {existingEventStyle(c, respondedAs)}"
                      title={tooltip}
                    >
                      {#if showInlineProp}
                        <div class="truncate leading-tight pr-10">{invite.summary || '(untitled)'}</div>
                        <span
                          class="absolute top-0.5 right-1 text-[9px] font-mono tabular-nums leading-none text-surface-500 dark:text-surface-400"
                        >{proposedRangeLabel}</span>
                      {:else}
                        <div class="truncate leading-tight">{invite.summary || '(untitled)'}</div>
                      {/if}
                      <!-- Invite badge — primary-tinted, sits at
                           the bottom-right so it doesn't clash
                           with the time pill on tall blocks and
                           still surfaces on short ones. -->
                      <span
                        class="absolute bottom-0 right-0 text-[8px] uppercase tracking-wider font-bold px-1 py-px rounded-tl bg-primary-500 text-white leading-none"
                        title="Matches this invite"
                      >✉ invite</span>
                    </div>
                  {:else}
                    <!-- Not-on-calendar branch: dashed primary
                         border + light primary tint signals
                         "this is what you would be adding". -->
                    <div
                      class="absolute rounded-sm text-[10px] font-medium border-2 border-dashed border-primary-500 bg-primary-500/15 text-primary-900 dark:text-primary-100 overflow-hidden pointer-events-auto"
                      style="left: 36px; right: 4px; top: {proposedGeom.top}px; height: {proposedGeom.height}px;"
                      title={tooltip}
                    >
                      {#if showInlineProp}
                        <div class="truncate leading-tight px-1 pr-10">{invite.summary || '(untitled)'}</div>
                        <span
                          class="absolute top-0.5 right-1 text-[9px] font-mono tabular-nums leading-none text-surface-500 dark:text-surface-400"
                        >{proposedRangeLabel}</span>
                      {:else}
                        <div class="truncate leading-tight px-1">{invite.summary || '(untitled)'}</div>
                      {/if}
                    </div>
                  {/if}
                {/if}

                {#if previewEvents.length === 0 && previewLoaded && !proposedGeom}
                  <div class="absolute inset-0 flex items-center justify-center text-[11px] text-surface-400 italic">
                    Nothing on this day
                  </div>
                {/if}
              </div>
            </div>
          </div>
        {/if}
      </div>
    </div>
  {/if}

</div>
{/if}

