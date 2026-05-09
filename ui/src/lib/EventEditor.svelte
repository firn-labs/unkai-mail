<script lang="ts">
  /**
   * EventEditor — modal for creating, editing, and deleting calendar
   * events.
   *
   * Mirrors the Compose modal pattern: full-screen dim overlay, a
   * fixed-size centered card with a sticky header / scrolling body /
   * sticky footer. The parent (`CalendarView`) opens it in one of two
   * shapes:
   *
   *   - **create**: pass `mode="create"` plus a `draft` (calendar id +
   *     start/end seeded from a `+ New event` click or a click-and-drag
   *     gesture in the time grid). The editor PUTs via
   *     `create_calendar_event`.
   *   - **edit**: pass `mode="edit"` plus the existing `event` and the
   *     calendar list (so we can show the calendar name in read-only
   *     form — moving an event between calendars is a follow-up). The
   *     editor PUTs via `update_calendar_event` and offers a Delete
   *     button (`delete_calendar_event`).
   *
   * On a successful save or delete we call `onsaved()` so the parent
   * can re-query the cache and repaint the grid; the modal closes
   * itself via `onclose()` either way.
   *
   * # Field mapping
   *
   * The form field shapes match the Rust `CalendarEventInput` struct
   * (camelCase via serde): `summary`, `description`, `location`,
   * `start`, `end`, `allDay`, `url`, `transparency`, `attendees`,
   * `reminders`. The attendee list is edited as a comma-separated
   * email string — power users can paste a list, and the parsed shape
   * round-trips through `CalendarEvent.attendees`.
   */

  import { convertFileSrc, invoke } from '@tauri-apps/api/core'
  import { formatError } from './errors'
  import Icon from './Icon.svelte'
  import DateField from './DateField.svelte'
  import TimeField from './TimeField.svelte'
  import Select from './Select.svelte'
  import EmailKindChip from './EmailKindChip.svelte'
  import Toggle from './Toggle.svelte'

  // ── Types (kept local; these mirror the Rust models) ──────────
  interface EventAttendee {
    email: string
    common_name?: string | null
    status?: string | null
    /** RFC 5545 ROLE: REQ-PARTICIPANT (Required) /
     *  OPT-PARTICIPANT (Optional) / CHAIR / NON-PARTICIPANT.
     *  Missing => Required. */
    role?: string | null
  }
  interface EventReminder {
    trigger_minutes_before: number
    action?: string | null
  }
  interface CalendarEvent {
    id: string
    summary: string
    description: string | null
    start: string
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
  interface CalendarSummary {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    /** CalDAV-derived read-only flag (#236).  Set when the
     *  user can only view this calendar (typical for shared
     *  calendars where the owner granted view-only access).
     *  The picker filters these out for new events and the
     *  Delete button is hidden when an existing event lives
     *  on one. */
    read_only?: boolean
  }

  // `stage` — used by Compose's "Add event" button (#152). Captures
  // the same fields as `create` but doesn't actually call
  // `create_calendar_event` on save: the parent (Compose) holds the
  // staged details and creates the event only after the email is
  // successfully sent, so a discarded mail leaves no orphaned event
  // behind.  A Talk room (when the user opted in) IS minted up-front
  // because its URL has to embed in the body — Compose deletes the
  // room on cancel via the existing rollback path.
  type Mode = 'create' | 'edit' | 'stage'

  /** Payload `onsaved` carries back after a successful create. Edit /
      delete still fire `onsaved()` with no argument — callers that
      don't care about the payload keep their `() => void` handler
      unchanged. */
  export interface SavedEvent {
    /** App-side composite event id returned by `create_calendar_event`.
     *  Empty string in `stage` mode where the event hasn't been
     *  persisted to CalDAV yet — Compose keeps the rest of the
     *  payload around and creates the event on send-success. */
    uid: string
    summary: string
    start: string
    end: string
    url: string | null
    /** Bare-address list of everyone invited.  Kept on the payload
        so callers can refresh related UI (badges, lists). */
    attendees: string[]
    /** Free-form location text. May hold a Talk room URL when the
     *  user opted into "Make it a Talk conversation" — callers
     *  that render a meeting invite to email (#195) inspect this
     *  to decide whether to surface a "Join Talk meeting" CTA. */
    location?: string | null
    /** Plain-text description / agenda. */
    description?: string | null
    /** Stage-mode only — the calendar id the user picked, so the
     *  parent can pass it through to `create_calendar_event` later. */
    calendarId?: string
    /** Stage-mode only — the raw input the editor would otherwise
     *  have handed to `create_calendar_event`.  Opaque to callers,
     *  round-tripped verbatim into the create call on send-success. */
    input?: unknown
    /** Stage-mode only — when the user opted into "Make it a Talk
     *  conversation", the room was minted up-front (its URL has
     *  to embed in the email body).  Compose tracks the token in
     *  its own cancel-time cleanup slot so a discarded mail
     *  deletes the orphan room. */
    talkRoomToken?: string | null
    talkRoomNcId?: string | null
  }

  interface Props {
    mode: Mode
    calendars: CalendarSummary[]
    /**
     * create-mode seed. Always carries the calendar id + start/end
     * (from a `+ New event` click or a click-and-drag on the grid).
     * The remaining content fields are optional prefills used by
     * callers that want to open the editor with more than just a
     * time slot — e.g. Compose's "Add event" action seeds summary
     * (from the email subject), attendees (from To/Cc), and url
     * (from a freshly created Talk room).
     */
    draft?: {
      calendarId: string
      start: Date
      end: Date
      allDay?: boolean
      summary?: string
      description?: string
      location?: string
      url?: string
      /** Each entry is a bare address or `"Name" <addr>` — same
          shape `parseAddress` accepts everywhere else.  All seeded
          here land as REQ-PARTICIPANT.  Use the per-role fields
          below when the caller wants a more nuanced split (e.g.
          MailView's "Respond with meeting" puts From / To in
          `requiredAttendees` and Cc in `optionalAttendees`). */
      attendees?: string[]
      requiredAttendees?: string[]
      optionalAttendees?: string[]
      chairAttendees?: string[]
      /** When true, the editor auto-creates a Talk room as soon as it
          mounts — same effect as clicking "Add Talk link", but the
          user doesn't have to.  Used by MailView's "Respond with
          meeting" so the meeting starts life with a join URL already
          attached. */
      createTalkRoom?: boolean
    } | null
    /** edit-mode subject: the existing event being edited. */
    event?: CalendarEvent | null
    onclose: () => void
    onsaved: (saved?: SavedEvent) => void
  }
  const {
    mode,
    calendars,
    draft,
    event,
    onclose,
    onsaved,
  }: Props = $props()

  // ── Form state ──────────────────────────────────────────────
  // Initial values are computed once at mount from `draft` (create) or
  // `event` (edit). After that the form is fully owned by these
  // `$state` cells, so further changes to the prop don't clobber the
  // user's typing.
  // svelte-ignore state_referenced_locally
  let summary = $state(event?.summary ?? draft?.summary ?? '')
  // svelte-ignore state_referenced_locally
  let description = $state(event?.description ?? draft?.description ?? '')
  // svelte-ignore state_referenced_locally
  let location = $state(event?.location ?? draft?.location ?? '')
  // svelte-ignore state_referenced_locally
  let transparency = $state(event?.transparency ?? 'OPAQUE')

  // Determine the starting calendar id. In edit-mode we derive it from
  // the event id (`{nc_id}::{cal_path}::…`). In create-mode the parent
  // hands us one explicitly; we override that with the user's
  // `default_calendar_id` setting if one is configured AND it points
  // at a calendar the editor knows about.
  // svelte-ignore state_referenced_locally
  let calendarId = $state(deriveInitialCalendarId())
  function deriveInitialCalendarId(): string {
    if (event) {
      const parts = event.id.split('::')
      return parts.slice(0, 2).join('::')
    }
    // Default for create-mode: prefer the parent's hint, else
    // the first writable calendar in the picker (#236 — used to
    // be `calendars[0]` which could land on a read-only calendar
    // and immediately fail the save).
    return (
      draft?.calendarId ??
      calendars.find((c) => !c.read_only)?.id ??
      calendars[0]?.id ??
      ''
    )
  }

  /** True when the currently-selected calendar is read-only (#236).
   *  Drives both "remove the Delete button" and the future
   *  Save-disabled UX so the user can't issue a write the server
   *  would reject anyway.  In edit-mode the calendar is fixed to
   *  the event's home calendar; in create-mode the picker filters
   *  read-only entries out so this stays `false` by construction. */
  const currentCalendarReadOnly = $derived.by(() => {
    const cal = calendars.find((c) => c.id === calendarId)
    return cal?.read_only === true
  })

  /** Picker options for create-mode (#236).  Read-only calendars
   *  are removed from the list entirely — saving would fail
   *  server-side and there's no point letting the user pick one
   *  in the first place.  Edit-mode renders the calendar as a
   *  static label, so this only affects the create flow. */
  const writableCalendars = $derived(
    calendars.filter((c) => !c.read_only),
  )

  // Close on Escape.  We attach to `document` so the key works
  // regardless of where focus is — including inside DateField /
  // TimeField / Select inputs.  Inner popovers (calendar grid,
  // time slots, dropdown listbox, attendee suggestions) render a
  // listbox or labelled dialog while open; if any of those exist,
  // their own Escape handler should close just the popover, so
  // we bail out and let them handle it instead of dismissing the
  // whole editor.
  $effect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key !== 'Escape') return
      if (
        document.querySelector(
          '[role="listbox"], [role="dialog"][aria-label="Pick a date"]',
        )
      ) {
        return
      }
      e.preventDefault()
      void cancel()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  })

  /** Cancel = onclose + best-effort cleanup of any Talk room we
      created during this session.  Only relevant in create-mode:
      the event was never saved, so the room would otherwise
      linger empty on the server.  Edit-mode keeps existing rooms
      untouched (the user might have created the room days ago and
      just opened the event to tweak the time). */
  async function cancel() {
    const room = pendingTalkRoom
    pendingTalkRoom = null
    if (mode === 'create' && room) {
      const cal = calendars.find((c) => c.id === calendarId)
      if (cal) {
        try {
          await invoke('delete_talk_room', {
            ncId: cal.nextcloud_account_id,
            roomToken: room.token,
          })
        } catch {
          // Best-effort — leaving an empty room behind is annoying
          // but not user-blocking, and the user can delete it from
          // TalkView if they care.
        }
      }
    }
    onclose()
  }

  // Async-load the default-calendar setting and switch to it if the
  // user hasn't manually changed the picker yet.  In create-mode
  // this is the single biggest UX nicety — the user's chosen
  // default calendar (or Nextcloud's `primary` if they haven't set
  // one) gets used reliably instead of whichever calendar happened
  // to come back first from the cache.
  $effect(() => {
    // create + stage modes both seed a fresh event — same
    // "default-calendar lookup + optional auto-Talk-room"
    // bootstrapping applies.  Edit mode opens an existing
    // event so neither step is appropriate.
    if (mode === 'edit') return
    void invoke<{ default_calendar_id: string | null }>('get_app_settings')
      .then((s) => {
        const def = s.default_calendar_id
        if (def && calendars.some((c) => c.id === def)) {
          calendarId = def
        }
      })
      .catch(() => {})
      .finally(() => {
        // Auto-mint a Talk room once the calendar selection has
        // settled — used by "Respond with meeting" and #152's
        // Compose Event button so the event mounts with a join
        // URL pre-attached.  Mirrors the manual "Add Talk link"
        // button so all the save-time wiring (participant
        // invites, public/private downgrade) reuses the same
        // path.
        if (draft?.createTalkRoom && !pendingTalkRoom && !creatingTalkRoom) {
          void addTalkLink()
        }
      })
  })

  // svelte-ignore state_referenced_locally
  const initialAllDay = inferAllDay()
  // svelte-ignore state_referenced_locally
  let allDay = $state(initialAllDay)

  // Date and time are kept as separate strings (#126) — one
  // `YYYY-MM-DD` and one `HH:MM` per endpoint — so we can render
  // a proper split picker that matches the mockup (#128).  We
  // seed from the event/draft (which carry UTC instants), but
  // surface them in the user's *local* zone so the inputs read
  // naturally; the save path combines + converts back to UTC.
  // For all-day events the date-only state is what matters and
  // the save path folds them into 00:00:00Z / 23:59:59Z.
  // svelte-ignore state_referenced_locally
  let startDate = $state(toLocalDateInput(initialStart()))
  // svelte-ignore state_referenced_locally
  let startTime = $state(toLocalTimeInput(initialStart()))
  // svelte-ignore state_referenced_locally
  let endDate = $state(toLocalDateInput(initialEnd()))
  // svelte-ignore state_referenced_locally
  let endTime = $state(toLocalTimeInput(initialEnd()))

  // ── Attendees ─────────────────────────────────────────────
  // Three role-bucketed lists drive the chip-row UI; the input
  // beneath each bucket adds *new* attendees with the matching
  // ROLE.  Existing attendees from `event` are seeded into
  // whichever bucket their ROLE points at, defaulting to
  // Required when the property is missing (RFC 5545 §3.2.18).
  // Each seed preserves its `common_name` / `status` (PARTSTAT)
  // so a save round-trip doesn't reset accepted/declined badges.
  type Role = 'REQ-PARTICIPANT' | 'OPT-PARTICIPANT' | 'CHAIR'
  function bucketFor(att: EventAttendee): Role {
    const r = (att.role ?? 'REQ-PARTICIPANT').toUpperCase()
    if (r === 'OPT-PARTICIPANT') return 'OPT-PARTICIPANT'
    if (r === 'CHAIR') return 'CHAIR'
    return 'REQ-PARTICIPANT'
  }
  function seedRole(list: string[] | undefined, role: Role): EventAttendee[] {
    return (list ?? [])
      .map((s) => parseAddressToAttendee(s, role))
      .filter((a): a is EventAttendee => !!a)
  }
  // svelte-ignore state_referenced_locally
  let requiredAttendees = $state<EventAttendee[]>(
    event
      ? (event.attendees ?? []).filter((a) => bucketFor(a) === 'REQ-PARTICIPANT')
      : [
          ...seedRole(draft?.attendees, 'REQ-PARTICIPANT'),
          ...seedRole(draft?.requiredAttendees, 'REQ-PARTICIPANT'),
        ],
  )
  // svelte-ignore state_referenced_locally
  let optionalAttendees = $state<EventAttendee[]>(
    event
      ? (event.attendees ?? []).filter((a) => bucketFor(a) === 'OPT-PARTICIPANT')
      : seedRole(draft?.optionalAttendees, 'OPT-PARTICIPANT'),
  )
  // svelte-ignore state_referenced_locally
  let chairAttendees = $state<EventAttendee[]>(
    event
      ? (event.attendees ?? []).filter((a) => bucketFor(a) === 'CHAIR')
      : seedRole(draft?.chairAttendees, 'CHAIR'),
  )

  /** Lower-cased addresses we consider "the user" — the union
   *  of every configured mail-account email.  Used by the RSVP
   *  dropdown to find the user's own ATTENDEE row in edit
   *  mode (when the user is invited to someone else's event)
   *  so they can change their response inline.  Loaded once
   *  on mount; an empty set just hides the RSVP dropdown
   *  (no harm done — the inbox card remains the canonical
   *  RSVP surface). */
  let userIdentities = $state<Set<string>>(new Set())
  $effect(() => {
    void invoke<{ email: string }[]>('get_accounts')
      .then((rows) => {
        const set = new Set<string>()
        for (const a of rows) if (a.email) set.add(a.email.toLowerCase())
        userIdentities = set
      })
      .catch(() => {})
  })

  /** Email of the Nextcloud user that owns the *currently
   *  selected* calendar.  This is the address NC's Mail Provider
   *  uses for iMIP, so it's the correct ORGANIZER for events
   *  saved into that calendar.  Refetched whenever the calendar
   *  picker changes (the user may have multiple NC servers
   *  configured with different emails). */
  let organizerEmail = $state<string | null>(null)
  // Cache by nc_id so swapping calendars doesn't re-hit OCS.
  const organizerCache = new Map<string, string | null>()
  $effect(() => {
    const cal = calendars.find((c) => c.id === calendarId)
    if (!cal) {
      organizerEmail = null
      return
    }
    const ncId = cal.nextcloud_account_id
    if (organizerCache.has(ncId)) {
      organizerEmail = organizerCache.get(ncId) ?? null
      return
    }
    void invoke<string | null>('get_nextcloud_user_email', { ncId })
      .then((email) => {
        organizerCache.set(ncId, email ?? null)
        // Only apply if the user hasn't switched calendars while
        // the OCS round-trip was in flight.
        const current = calendars.find((c) => c.id === calendarId)
        if (current?.nextcloud_account_id === ncId) {
          organizerEmail = email ?? null
        }
      })
      .catch(() => {
        organizerCache.set(ncId, null)
      })
  })

  // In create mode, auto-add the NC user (calendar owner) as
  // CHAIR (organizer) once their email is known.  Skip if
  // they're already in any bucket — e.g. seeded from the
  // originating email's To/Cc, or already added on a previous
  // calendar switch.
  $effect(() => {
    if (mode !== 'create') return
    if (!organizerEmail) return
    const me = organizerEmail.toLowerCase()
    const present = [...requiredAttendees, ...optionalAttendees, ...chairAttendees]
      .some((a) => a.email.toLowerCase() === me)
    if (present) return
    chairAttendees = [
      ...chairAttendees,
      { email: organizerEmail, role: 'CHAIR', status: 'ACCEPTED' },
    ]
  })
  /** The user's own ATTENDEE row in this event (if any).  When
   *  set, the RSVP dropdown surfaces and is bound to its
   *  PARTSTAT.  Searches all three buckets so it works for
   *  users who were typed as Hosts / Required / Optional. */
  let myAttendee = $derived.by(() => {
    if (mode !== 'edit') return null
    if (userIdentities.size === 0) return null
    const all = [...requiredAttendees, ...optionalAttendees, ...chairAttendees]
    for (const a of all) {
      if (userIdentities.has(a.email.toLowerCase())) return a
    }
    return null
  })

  /** PARTSTAT the user's own ATTENDEE row carried when the
   *  editor opened — captured once from the original `event`
   *  prop so the save flow can detect "did the user change
   *  their RSVP?" without depending on `myAttendee`'s mutable
   *  bucket-array reactivity.  Stays `null` for create mode and
   *  for edit-mode events where the user isn't an attendee. */
  // svelte-ignore state_referenced_locally
  let originalUserPartstat = $state<string | null>(deriveOriginalUserPartstat())
  function deriveOriginalUserPartstat(): string | null {
    if (mode !== 'edit' || !event) return null
    for (const a of event.attendees ?? []) {
      // userIdentities may not be populated yet on first paint;
      // we re-resolve once it lands via the effect below.
      if (userIdentities.has(a.email.toLowerCase())) {
        return (a.status ?? 'NEEDS-ACTION').toUpperCase()
      }
    }
    return null
  }
  // userIdentities loads async — re-derive once it's known so the
  // change detection has the right baseline.
  $effect(() => {
    if (originalUserPartstat !== null) return
    if (userIdentities.size === 0) return
    originalUserPartstat = deriveOriginalUserPartstat()
  })

  // One pending input per role.  Each commits to its bucket on
  // Enter / comma / blur.  Datalist suggestions come from the
  // shared address book cache (loaded lazily once on mount).
  let requiredInput = $state('')
  let optionalInput = $state('')
  let chairInput = $state('')

  /** Contact row mirroring the Rust `search_contacts` payload —
   *  `id` is what the `contact-photo://` URI scheme keys off,
   *  `email[]` is the typed-and-kinded list of vCard EMAIL
   *  values, and `photo_mime` tells us whether to render the
   *  photo or fall back to an initials bubble. */
  interface ContactEmail {
    kind: string
    value: string
  }
  interface Contact {
    id: string
    display_name: string
    email: ContactEmail[]
    organization: string | null
    photo_mime: string | null
  }
  /** Cached contact list keyed by lowercase email so chips —
   *  which only know the email — can render the matching photo
   *  + display name without an extra IPC round-trip per chip. */
  let contactsByEmail = $state<Map<string, Contact>>(new Map())
  $effect(() => {
    void invoke<Contact[]>('search_contacts', { query: '', limit: 500 })
      .then((rows) => {
        const map = new Map<string, Contact>()
        for (const c of rows) {
          for (const e of c.email) {
            if (e.value) map.set(e.value.toLowerCase(), c)
          }
        }
        contactsByEmail = map
      })
      .catch(() => {})
  })

  /** Pick the first non-empty email from a Contact. */
  function primaryEmail(c: Contact): string {
    return c.email.find((e) => e.value.length > 0)?.value ?? ''
  }

  /** `<img src>` against the custom `contact-photo://` scheme.
   *  Falls back to `null` when the contact doesn't have a vCard
   *  photo so chips/dropdown render an initials bubble. */
  function photoUrl(c: Contact | undefined): string | null {
    if (!c || !c.photo_mime) return null
    return convertFileSrc(c.id, 'contact-photo')
  }

  function initials(name: string): string {
    const parts = name.trim().split(/\s+/).filter(Boolean)
    if (parts.length === 0) return '?'
    if (parts.length === 1) return parts[0][0].toUpperCase()
    return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase()
  }

  // ── Internal-vs-external resolution ──────────────────────
  // Each attendee email is looked up once against NC's
  // `sharees` endpoint; a hit means the address belongs to a
  // local NC principal, which:
  //   - flips the chip's "internal" badge on,
  //   - lets the save flow add them to the Talk room as a
  //     `users` participant (in-NC notification) rather than
  //     an `emails` guest,
  //   - drives the room-type decision (all-internal => private).
  // `null` means "external"; `undefined` means "haven't asked
  // yet" — the UI stays neutral until the lookup resolves.
  interface InternalUser {
    user_id: string
    display_name: string
  }
  let internalLookup = $state<Map<string, InternalUser | null>>(new Map())

  /** Resolve any unknown attendee emails against NC's
   *  user-search.  Best-effort: failures cache as `null`
   *  (treated as external) so the lookup doesn't retry every
   *  effect cycle.  Fires whenever the bucket lists change. */
  $effect(() => {
    const all = [...requiredAttendees, ...optionalAttendees, ...chairAttendees]
    const cal = calendars.find((c) => c.id === calendarId)
    if (!cal) return
    const ncId = cal.nextcloud_account_id
    for (const att of all) {
      const key = att.email.toLowerCase()
      if (internalLookup.has(key)) continue
      // Mark as in-flight (null sentinel — replaced when the
      // OCS reply lands).  Without this guard, a re-render
      // before the promise resolves would re-fire the lookup.
      internalLookup.set(key, null)
      void invoke<InternalUser | null>('find_nextcloud_user_by_email', {
        ncId,
        email: att.email,
      })
        .then((m) => {
          internalLookup.set(key, m ?? null)
          // Trigger reactivity: replacing the Map ref forces
          // chip rows to re-render with the new badge state.
          internalLookup = new Map(internalLookup)
        })
        .catch((e) => {
          console.warn('find_nextcloud_user_by_email failed', e)
          internalLookup.set(key, null)
        })
    }
  })

  function isInternal(email: string): boolean {
    return !!internalLookup.get(email.toLowerCase())
  }

  // ── Per-role inline suggestion dropdown ──────────────────
  // Each role's input has its own debounced `search_contacts`
  // query and its own dropdown state — same plumbing
  // `AddressAutocomplete` uses on Compose's To/Cc/Bcc, but
  // adapted to the chip-based commit model: clicking a
  // suggestion adds the contact directly to the role bucket
  // instead of stuffing its address into a comma-separated
  // string.
  interface MailingListSuggestion {
    id: string
    source: 'category' | 'team' | 'manual'
    name: string
    members: { displayName: string; email: string }[]
    hiddenFromAutocomplete: boolean
    emoji: string | null
  }
  type Suggestion =
    | { kind: 'contact'; contact: Contact; email: ContactEmail }
    | { kind: 'list'; list: MailingListSuggestion }
  let activeSuggestionRole = $state<Role | null>(null)
  let dropdownSuggestions = $state<Suggestion[]>([])
  let activeIndex = $state(0)
  const SEARCH_DEBOUNCE_MS = 150
  const SUGGESTION_LIMIT = 8
  let searchDebounce: number | null = null
  /** Mailing lists (categories + manual KIND:group + Teams),
   *  fetched once and filtered client-side on every keystroke.
   *  Hidden-from-autocomplete rows stay out of the dropdown. */
  let allLists = $state<MailingListSuggestion[]>([])
  void invoke<MailingListSuggestion[]>('list_mailing_lists')
    .then((rows) => {
      allLists = rows.filter((m) => !m.hiddenFromAutocomplete)
    })
    .catch((e) => console.warn('list_mailing_lists failed', e))

  function runSuggestionSearch(role: Role, query: string) {
    const q = query.trim()
    if (q.length < 2) {
      dropdownSuggestions = []
      activeSuggestionRole = null
      return
    }
    if (searchDebounce !== null) window.clearTimeout(searchDebounce)
    searchDebounce = window.setTimeout(async () => {
      try {
        const rows = await invoke<Contact[]>('search_contacts', {
          query: q,
          limit: SUGGESTION_LIMIT,
        })
        // Stale-response guard: only commit if this role is
        // still the focused one.
        if (activeSuggestionRole !== role) return
        const ql = q.toLowerCase()
        const listHits = allLists
          .filter((m) => m.name.toLowerCase().includes(ql))
          .slice(0, SUGGESTION_LIMIT)
        // Expand each contact into one suggestion per email so
        // multi-address contacts (Home + Work) surface every
        // address in the dropdown.
        const contactSuggestions: Suggestion[] = []
        for (const c of rows) {
          const emails = c.email.filter((e) => e.value.length > 0)
          if (emails.length === 0) continue
          for (const e of emails) {
            contactSuggestions.push({ kind: 'contact', contact: c, email: e })
          }
        }
        const merged: Suggestion[] = [
          ...listHits.map((m) => ({ kind: 'list' as const, list: m })),
          ...contactSuggestions,
        ]
        dropdownSuggestions = merged.slice(0, SUGGESTION_LIMIT)
        activeIndex = 0
      } catch (e) {
        console.warn('search_contacts failed', e)
        dropdownSuggestions = []
      }
    }, SEARCH_DEBOUNCE_MS)
  }

  function clearSuggestionInput(role: Role) {
    if (role === 'REQ-PARTICIPANT') requiredInput = ''
    else if (role === 'OPT-PARTICIPANT') optionalInput = ''
    else chairInput = ''
    activeSuggestionRole = null
    dropdownSuggestions = []
  }

  function appendAttendees(role: Role, atts: EventAttendee[]) {
    if (atts.length === 0) return
    if (role === 'REQ-PARTICIPANT') {
      requiredAttendees = [...requiredAttendees, ...atts]
    } else if (role === 'OPT-PARTICIPANT') {
      optionalAttendees = [...optionalAttendees, ...atts]
    } else {
      chairAttendees = [...chairAttendees, ...atts]
    }
  }

  /** Add a contact or mailing list directly to a role bucket
   *  via the suggestion dropdown.  Skips entries already in
   *  any bucket; lists expand member-by-member. */
  function pickSuggestion(role: Role, s: Suggestion) {
    if (s.kind === 'contact') {
      const c = s.contact
      const addr = s.email.value
      clearSuggestionInput(role)
      if (!addr) return
      const exists = [...requiredAttendees, ...optionalAttendees, ...chairAttendees].some(
        (a) => a.email.toLowerCase() === addr.toLowerCase(),
      )
      if (exists) return
      appendAttendees(role, [{ email: addr, common_name: c.display_name || null, role }])
      // Mirror into the by-email cache so the chip avatar
      // resolves immediately (don't wait for the cache reload).
      contactsByEmail.set(addr.toLowerCase(), c)
      contactsByEmail = new Map(contactsByEmail)
    } else {
      const list = s.list
      clearSuggestionInput(role)
      const seen = new Set(
        [...requiredAttendees, ...optionalAttendees, ...chairAttendees].map((a) =>
          a.email.toLowerCase(),
        ),
      )
      const adds: EventAttendee[] = []
      for (const m of list.members) {
        if (!m.email) continue
        const key = m.email.toLowerCase()
        if (seen.has(key)) continue
        seen.add(key)
        adds.push({ email: m.email, common_name: m.displayName || null, role })
      }
      appendAttendees(role, adds)
    }
  }

  /** Parse a single piece into an attendee with the given role.
   *  Accepts `"Name" <addr>`, `Name <addr>`, or a bare `addr`.
   *  Returns null for empty / malformed pieces. */
  function parseAddressToAttendee(
    piece: string,
    role: Role,
  ): EventAttendee | null {
    const trimmed = piece.trim()
    if (!trimmed) return null
    const m = trimmed.match(/^\s*(?:"([^"]*)"|([^<]*?))\s*<([^>]+)>\s*$/)
    if (m) {
      const name = (m[1] ?? m[2] ?? '').trim().replace(/\\"/g, '"')
      return {
        email: m[3].trim(),
        common_name: name || null,
        role,
      }
    }
    // Bare email — also accept lookups against the cached
    // contacts so a user who types just the display name and
    // then commits (Enter / comma / blur) without using the
    // dropdown still picks up the matching email automatically.
    //
    // Guard against the temporal dead zone: this function is
    // *also* invoked from the initial `$state` seeders for
    // `requiredAttendees` / `optionalAttendees` / `chairAttendees`
    // up at the top of the script, which run BEFORE
    // `contactsByEmail` is declared further down.  Reaching the
    // for-loop with a bare-email piece (the recipient of an
    // already-answered thread, for example, where the From header
    // came back as a plain `user@host`) used to throw
    // `Cannot access 'contactsByEmail' before initialization` and
    // crash EventEditor's mount silently.  Skipping the lookup at
    // init time is harmless: the draft we're seeding from already
    // carries email addresses, not display names, so the
    // display-name-to-email match wouldn't help anyway.  User-
    // typed bare-email pieces (the original use case for this
    // lookup) reach this function long after init, so the
    // try/catch falls through to the real Map.
    let contactsLookup: Map<string, Contact> | null = null
    try {
      contactsLookup = contactsByEmail
    } catch {
      contactsLookup = null
    }
    if (contactsLookup) {
      for (const c of contactsLookup.values()) {
        if ((c.display_name ?? '').toLowerCase() === trimmed.toLowerCase()) {
          return { email: primaryEmail(c), common_name: c.display_name, role }
        }
      }
    }
    return { email: trimmed, common_name: null, role }
  }

  /** Add a comma- / semicolon-separated batch of pieces from
   *  one of the three role inputs.  Splits on `,` / `;`, dedupes
   *  by lowercase email across *all* buckets so the same
   *  address can't be both Required and Optional. */
  function commitInput(role: Role) {
    const text =
      role === 'REQ-PARTICIPANT'
        ? requiredInput
        : role === 'OPT-PARTICIPANT'
          ? optionalInput
          : chairInput
    if (!text.trim()) return
    const seen = new Set(
      [...requiredAttendees, ...optionalAttendees, ...chairAttendees].map((a) =>
        a.email.toLowerCase(),
      ),
    )
    const adds: EventAttendee[] = []
    for (const piece of text.split(/[,;]/)) {
      const att = parseAddressToAttendee(piece, role)
      if (!att) continue
      const key = att.email.toLowerCase()
      if (seen.has(key)) continue
      seen.add(key)
      adds.push(att)
    }
    if (adds.length === 0) {
      // Field had only invalid input — clear it so the user can
      // try again.
      if (role === 'REQ-PARTICIPANT') requiredInput = ''
      else if (role === 'OPT-PARTICIPANT') optionalInput = ''
      else chairInput = ''
      return
    }
    if (role === 'REQ-PARTICIPANT') {
      requiredAttendees = [...requiredAttendees, ...adds]
      requiredInput = ''
    } else if (role === 'OPT-PARTICIPANT') {
      optionalAttendees = [...optionalAttendees, ...adds]
      optionalInput = ''
    } else {
      chairAttendees = [...chairAttendees, ...adds]
      chairInput = ''
    }
  }

  function removeAttendee(role: Role, email: string) {
    if (role === 'REQ-PARTICIPANT') {
      requiredAttendees = requiredAttendees.filter((a) => a.email !== email)
    } else if (role === 'OPT-PARTICIPANT') {
      optionalAttendees = optionalAttendees.filter((a) => a.email !== email)
    } else {
      chairAttendees = chairAttendees.filter((a) => a.email !== email)
    }
  }

  /** Render an attendee for the chip label.  When the row's
   *  email matches one of the user's configured mail-account
   *  identities, render "You" — that's the address the invite
   *  landed on, and the user reads "You" much faster than their
   *  own email at a glance.  Otherwise prefer the CN, falling
   *  back to the bare email. */
  function chipLabel(a: EventAttendee): string {
    if (userIdentities.has(a.email.toLowerCase())) return 'You'
    if (a.common_name && a.common_name.trim() && a.common_name !== a.email) {
      return a.common_name
    }
    return a.email
  }

  // Reminder picker: a single dropdown that maps to the most common
  // VALARM offsets. "Custom" preserves whatever multi-alarm setup the
  // event came in with (we keep `originalReminders` for that case).
  type ReminderChoice =
    | 'none'
    | '0'
    | '5'
    | '10'
    | '15'
    | '30'
    | '45'
    | '60'
    | '120'
    | '1440'
    | '10080'
    | 'custom'
  // svelte-ignore state_referenced_locally
  let reminderChoice = $state<ReminderChoice>(deriveReminderChoice())
  // svelte-ignore state_referenced_locally
  const originalReminders = event?.reminders ?? []
  function deriveReminderChoice(): ReminderChoice {
    const list = event?.reminders ?? []
    if (list.length === 0) {
      // New events default to "15 minutes before" (#236).
      // Edit-mode events with no VALARM stay on 'none' — the
      // user explicitly didn't ask for a reminder, and silently
      // adding one would be a behaviour change for every event
      // they re-save.
      return event ? 'none' : '15'
    }
    if (list.length > 1) return 'custom'
    const m = list[0].trigger_minutes_before
    // Preset minutes-before values that map cleanly to dropdown
    // labels.  Anything else falls through to 'custom' and is
    // round-tripped via `originalReminders` so the user's odd
    // 17-min reminder doesn't silently become a 15-min one on
    // re-save.  120 (2h) and 10080 (1w) added in #236 — without
    // them, existing events with those reminders showed up as
    // "Custom" and looked like the dropdown wasn't loading.
    if (
      m === 0 ||
      m === 5 ||
      m === 10 ||
      m === 15 ||
      m === 30 ||
      m === 45 ||
      m === 60 ||
      m === 120 ||
      m === 1440 ||
      m === 10080
    ) {
      return String(m) as ReminderChoice
    }
    return 'custom'
  }

  let saving = $state(false)
  let deleting = $state(false)
  let error = $state('')

  // ── Initial conversions ─────────────────────────────────────
  function initialStart(): Date {
    if (event) return new Date(event.start)
    if (draft) return draft.start
    const now = new Date()
    now.setMinutes(0, 0, 0)
    return now
  }
  function initialEnd(): Date {
    if (event) return new Date(event.end)
    if (draft) return draft.end
    const out = initialStart()
    out.setHours(out.getHours() + 1)
    return out
  }
  function inferAllDay(): boolean {
    if (draft?.allDay) return true
    if (!event) return false
    // Same heuristic as CalendarView.isAllDay — UTC midnight start,
    // span ≈ N×24h.
    const s = new Date(event.start)
    const e = new Date(event.end)
    if (s.getUTCHours() !== 0 || s.getUTCMinutes() !== 0) return false
    const hours = (e.getTime() - s.getTime()) / 3_600_000
    if (hours < 23) return false
    const remainder = hours % 24
    return remainder < 1 / 60 || remainder > 24 - 1 / 60
  }

  // ── datetime / date helpers ─────────────────────────────────
  /** `Date` → `YYYY-MM-DD` in the user's *local* zone (split-
   *  picker date half).  Note this is local-zone unlike the
   *  earlier all-day-only `toDateInput` helper which was UTC-
   *  based — both timed and all-day flows now go through the
   *  same date string and the save path interprets it
   *  appropriately. */
  function toLocalDateInput(d: Date): string {
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
  }
  /** `Date` → `HH:MM` in the user's local zone. */
  function toLocalTimeInput(d: Date): string {
    const pad = (n: number) => String(n).padStart(2, '0')
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`
  }
  /** Combine a local `YYYY-MM-DD` + `HH:MM` into a `Date`. */
  function fromLocalSplit(date: string, time: string): Date {
    // Browsers parse `YYYY-MM-DDTHH:MM` (no offset) as local
    // time, then the Date constructor stores it as UTC under
    // the hood.  Save-path callers then `.toISOString()` it.
    return new Date(`${date}T${time || '00:00'}`)
  }
  /** `YYYY-MM-DD` (treated as a UTC calendar date) → midnight UTC. */
  function dateInputToUtcMidnight(s: string): Date {
    return new Date(`${s}T00:00:00Z`)
  }
  /** `YYYY-MM-DD` → 23:59:59.999 UTC of that date. */
  function dateInputToUtcEndOfDay(s: string): Date {
    return new Date(`${s}T23:59:59.999Z`)
  }

  // When the user toggles all-day, keep the visible inputs sane:
  // turning it on, the date stays; turning it off, restore a
  // sensible 09:00 → 10:00 window in local time.
  function onToggleAllDay() {
    if (!allDay) {
      // Just turned timed: reset times to a 09:00 → 10:00 slot
      // on the same date.
      if (!startTime) startTime = '09:00'
      if (!endTime) endTime = '10:00'
    }
  }

  // ── Save / delete ───────────────────────────────────────────
  /** Flatten the three role-bucketed attendee lists into the
   *  single array the backend expects.  Each bucket already
   *  carries the right `role`; PARTSTAT (`status`) is preserved
   *  on existing rows because the buckets were seeded from the
   *  inbound event verbatim.  Any pending text in the inputs is
   *  flushed first so an unsaved last word doesn't get dropped. */
  function buildAttendees(): EventAttendee[] {
    if (requiredInput.trim()) commitInput('REQ-PARTICIPANT')
    if (optionalInput.trim()) commitInput('OPT-PARTICIPANT')
    if (chairInput.trim()) commitInput('CHAIR')
    return [...requiredAttendees, ...optionalAttendees, ...chairAttendees]
  }

  function buildReminders(): EventReminder[] {
    if (reminderChoice === 'none') return []
    if (reminderChoice === 'custom') return originalReminders
    return [
      {
        trigger_minutes_before: parseInt(reminderChoice, 10),
        action: 'DISPLAY',
      },
    ]
  }

  // ── Talk meeting button ───────────────────────────────────
  // Mints a fresh Nextcloud Talk room on the calendar's parent
  // Nextcloud account, then writes the room URL into the
  // event's LOCATION field — same destination Nextcloud
  // Calendar's own "Make it a Talk conversation" button uses
  // (`AddTalkModal.vue` $emits `updateLocation`).  Putting the
  // URL on LOCATION instead of URL is what triggers NC's
  // Calendar UI to render the "Join Talk conversation"
  // affordance, and what makes NC's iMIP template surface the
  // call link in the invite mail's body.
  //
  // Falls back to appending the URL to DESCRIPTION when
  // LOCATION is already set, again matching NC's modal
  // (lines 247-257 in AddTalkModal.vue).  Best-effort: a
  // failure surfaces in the local `error` banner but doesn't
  // block saving the event.
  /** Sync the editor's attendee list with the Talk room
   *  created from this editor session.  For each attendee we
   *  resolve internal-vs-external (using the cached lookup
   *  from `internalLookup`, falling back to a fresh OCS query
   *  for any address whose lookup hasn't landed yet) and POST
   *  to Talk's `participants` endpoint with `users` or
   *  `emails` source accordingly:
   *    - `users` → in-NC notification, the participant joins
   *      authenticated.
   *    - `emails` → Talk emails them a guest URL alongside
   *      the calendar invite NC's iMIP plugin already sends.
   *  Then, if every attendee turned out internal, downgrade
   *  the room from public to private — externals don't need
   *  the guest-URL escape hatch any more.  Best-effort: each
   *  POST that fails is logged (per-attendee) and we keep
   *  going. */
  async function syncTalkParticipants(attendees: EventAttendee[]) {
    const room = pendingTalkRoom
    if (!room) return
    const cal = calendars.find((c) => c.id === calendarId)
    if (!cal) return
    const ncId = cal.nextcloud_account_id

    // Drop the user's own ATTENDEE row before talking to Talk —
    // they're already the room's auto-owner (NC adds the
    // creator), and the EventEditor auto-seeds the user as a
    // CHAIR (organiser) on every new event since #128.  Without
    // this filter that CHAIR row would land in the room a second
    // time as either a `users` participant or — if the lookup
    // misses — a `Email` guest with the user's own address.
    attendees = attendees.filter(
      (a) => !userIdentities.has(a.email.toLowerCase()),
    )

    // Fill any gaps in `internalLookup` synchronously here so
    // the room-type decision is based on the *full* answer set
    // — without this guard, an attendee added seconds before
    // Save would still register as undefined and we'd
    // pessimistically leave the room public.
    const lookups: Promise<void>[] = []
    for (const att of attendees) {
      const key = att.email.toLowerCase()
      if (internalLookup.has(key)) continue
      internalLookup.set(key, null)
      lookups.push(
        invoke<InternalUser | null>('find_nextcloud_user_by_email', {
          ncId,
          email: att.email,
        })
          .then((m) => {
            internalLookup.set(key, m ?? null)
          })
          .catch(() => {
            internalLookup.set(key, null)
          }),
      )
    }
    await Promise.all(lookups)

    let allInternal = attendees.length > 0
    for (const att of attendees) {
      const match = internalLookup.get(att.email.toLowerCase())
      const participant = match
        ? { kind: 'user' as const, value: match.user_id }
        : { kind: 'email' as const, value: att.email }
      if (!match) allInternal = false
      try {
        await invoke('add_talk_participant', {
          ncId,
          roomToken: room.token,
          participant,
        })
      } catch (e) {
        // Non-fatal — Talk returns 4xx when the participant is
        // already on the room (e.g. the user re-saved an event
        // they'd already invited people to), which we treat
        // as a no-op.
        console.warn('add_talk_participant failed', att.email, e)
      }
    }

    // Toggle visibility iff the desired state differs from the
    // last-known one.  An all-internal attendee set means we
    // can switch to private (the URL-only join is no longer
    // needed); any external attendee keeps it public.
    const desiredPublic = !allInternal
    if (desiredPublic !== room.isPublic) {
      try {
        await invoke('set_talk_room_public', {
          ncId,
          roomToken: room.token,
          public: desiredPublic,
        })
        pendingTalkRoom = { ...room, isPublic: desiredPublic }
      } catch (e) {
        console.warn('set_talk_room_public failed', e)
      }
    }
  }

  // Tracks the Talk room created from this editor session so
  // the save flow can post per-attendee participants and (when
  // every attendee turned out internal) downgrade the room
  // from public to private.  Cleared when the URL is no longer
  // present in LOCATION (the user manually wiped it).
  interface PendingTalkRoom {
    token: string
    web_url: string
    /** Last-known visibility we set on the server so the save
     *  flow only PATCHes when the desired state actually
     *  changed — avoids spurious round-trips on every save. */
    isPublic: boolean
  }
  // svelte-ignore state_referenced_locally
  let pendingTalkRoom = $state<PendingTalkRoom | null>(null)
  let creatingTalkRoom = $state(false)
  async function addTalkLink() {
    error = ''
    const cal = calendars.find((c) => c.id === calendarId)
    if (!cal) {
      error = 'Pick a calendar before creating a Talk room.'
      return
    }
    creatingTalkRoom = true
    try {
      // Room name = event title when present, falling back to a
      // generic label.  Talk rejects empty names and clamps long
      // ones; "Meeting" is the same default the NC Calendar app
      // uses for unnamed rooms.
      const roomName = summary.trim() || 'Meeting'
      const room = await invoke<{ token: string; web_url: string }>('create_talk_room', {
        ncId: cal.nextcloud_account_id,
        roomName,
        // No participants up-front — they're resolved + added
        // on save once the user has finished typing the
        // attendee list.
        participants: [],
        // Mirror Nextcloud Calendar's "Make it a Talk
        // conversation" button: tag the room as event-bound so
        // Talk's UI categorises it as a meeting room (filtered
        // out of "select existing conversation" lists for other
        // events).  The id is random — NC Calendar itself uses
        // md5(Date.now()), not the iCal UID, so there's no real
        // foreign-key here, just a tag.
        objectType: 'event',
        objectId: crypto.randomUUID(),
        // Public by default so externals invited to the event
        // can join via the calendar URL without an NC login.
        // The save flow downgrades to private when every
        // attendee resolves internal.
        roomType: 3,
      })
      pendingTalkRoom = { token: room.token, web_url: room.web_url, isPublic: true }
      if (!location.trim()) {
        location = room.web_url
      } else {
        // NC's modal does the same: separator + the URL appended
        // to the existing description.  Avoids clobbering any
        // location the user already typed.
        description = description.trim()
          ? `${description}\n\n${room.web_url}`
          : room.web_url
      }
    } catch (e) {
      error = `Failed to create Talk room: ${formatError(e) || e}`
    } finally {
      creatingTalkRoom = false
    }
  }

  function buildInput() {
    const start = allDay
      ? dateInputToUtcMidnight(startDate)
      : fromLocalSplit(startDate, startTime)
    const end = allDay
      ? dateInputToUtcEndOfDay(endDate)
      : fromLocalSplit(endDate, endTime)
    return {
      summary: summary.trim(),
      description: description.trim() ? description.trim() : null,
      location: location.trim() ? location.trim() : null,
      start: start.toISOString(),
      end: end.toISOString(),
      allDay,
      // URL field on iCalendar events isn't surfaced as a
      // first-class control any more — Talk meetings write
      // their join link into LOCATION (matching what
      // Nextcloud Calendar's "Make it a Talk conversation"
      // button does).  Pass null so the backend doesn't carry
      // a stale value forward in edit mode either.
      url: null,
      transparency: transparency || null,
      attendees: buildAttendees(),
      reminders: buildReminders(),
    }
  }

  async function save() {
    error = ''
    if (!summary.trim()) {
      error = 'Title is required'
      return
    }
    if (!calendarId) {
      error = 'Pick a calendar'
      return
    }
    const input = buildInput()
    // Reject inverted ranges before bothering the backend.
    if (new Date(input.end).getTime() <= new Date(input.start).getTime()) {
      error = 'End must be after start'
      return
    }
    saving = true
    // ORGANIZER on the CalDAV PUT is resolved server-side from
    // the user's NC profile email (see `create_calendar_event`
    // in main.rs).  That address is what NC's Mail Provider
    // matches against the configured Mail-app account to pick
    // the SMTP for outbound iMIP — so it MUST be the user's
    // real address, not a synthetic hostname-based one.  No
    // override threading from the editor any more; the editor
    // doesn't know which mail account is "primary" and the
    // backend has authoritative information.
    try {
      if (mode === 'create') {
        const created = await invoke<{ id: string }>('create_calendar_event', {
          calendarId,
          input,
        })
        onsaved({
          uid: created.id,
          summary: input.summary,
          start: input.start,
          end: input.end,
          url: null,
          attendees: input.attendees.map((a) => a.email),
          location: input.location,
          description: input.description,
        })
      } else if (mode === 'stage') {
        // #152 — Compose-driven staging.  Skip the CalDAV
        // create entirely; the parent will call
        // `create_calendar_event` with the same `input` once
        // the user successfully sends the mail.  We still
        // report the staged details via `onsaved` (with `uid`
        // empty as the "not yet persisted" sentinel) so the
        // parent can render the meeting card / track Talk
        // room cleanup.  We hand off the Talk room token (when
        // one was minted via the in-editor toggle) so Compose
        // can register it in its own cancel-time cleanup slot
        // and delete the orphan room on Discard — the event
        // itself was never persisted, but the Talk room was.
        const cal = calendars.find((c) => c.id === calendarId)
        onsaved({
          uid: '',
          summary: input.summary,
          start: input.start,
          end: input.end,
          url: null,
          attendees: input.attendees.map((a) => a.email),
          location: input.location,
          description: input.description,
          calendarId,
          input,
          talkRoomToken: pendingTalkRoom?.token ?? null,
          talkRoomNcId: cal?.nextcloud_account_id ?? null,
        })
        // Clear the editor's own pending-room slot so its own
        // cancel-time cleanup doesn't fire — Compose owns the
        // room from here on.
        pendingTalkRoom = null
      } else if (event) {
        // RSVP-only fast path: when the user is themselves an
        // attendee and changed their PARTSTAT, route the update
        // through the dedicated surgical IPC.  That preserves
        // the cached body byte-for-byte (just flipping the user's
        // ATTENDEE PARTSTAT and stamping SCHEDULE-FORCE-SEND=
        // REPLY) so Sabre's iTIP broker classifies the diff as a
        // genuine RSVP and dispatches the REPLY iMIP.  Going
        // through `update_calendar_event` would regenerate the
        // body from form fields and the broker silently swallows
        // the iMIP — issue #124 / regression from #128.
        const newPartstat = (myAttendee?.status ?? 'NEEDS-ACTION').toUpperCase()
        const partstatChanged =
          myAttendee !== null &&
          originalUserPartstat !== null &&
          newPartstat !== originalUserPartstat
        if (partstatChanged) {
          await invoke('rsvp_existing_event', {
            eventId: event.id,
            partstat: newPartstat,
            attendeeHint: myAttendee?.email ?? null,
          })
          // Pin the new baseline so a subsequent save in the same
          // session doesn't try to fire another surgical RSVP for
          // a PARTSTAT that already landed.
          originalUserPartstat = newPartstat
        } else {
          await invoke('update_calendar_event', {
            eventId: event.id,
            input,
          })
        }
        onsaved()
      }

      // Dismiss IMMEDIATELY now that the calendar event is
      // persisted (#255 follow-up).  Awaiting Talk-participant
      // sync inside the same try-block kept the editor — and the
      // closure holding `onclose` — alive for the 1-3 s of
      // sequential NC round-trips, and any late `onclose()` would
      // race against the parent's next "open editor" gesture
      // (e.g. a second "Respond with meeting" reply on the same
      // thread).  The parent's `onsaved` already fired above, so
      // the editor has done everything it owes the caller; the
      // Talk-participant sync is post-success housekeeping that
      // the user shouldn't have to wait on.
      onclose()

      // Fire-and-forget Talk participant sync.  Only fires when
      // the user created a Talk room from this editor session
      // and there's at least one attendee — pure "Talk-but-no-
      // attendees" rooms behave like personal scratch rooms and
      // don't need any of this.  Errors land in the console; the
      // calendar event is already saved and the user has moved
      // on.  Misses get caught the next time the user opens the
      // event in the editor (the room sync re-runs from there).
      if (pendingTalkRoom && input.attendees.length > 0) {
        void syncTalkParticipants(input.attendees).catch((e) =>
          console.warn('syncTalkParticipants failed (event already saved)', e),
        )
      }
    } catch (e) {
      error = formatError(e) || 'Failed to save event'
    } finally {
      saving = false
    }
  }

  /** Pull a `/call/<token>` token out of an arbitrary string.
      Matches both pretty (`/call/abc`) and ugly (`/index.php/call/abc`)
      URLs — Talk uses one or the other depending on the server's
      `htaccess` setup.  Returns null when nothing looks like a Talk
      URL, or when the token wouldn't be a valid identifier
      (length-bounded so a stray `/call/` followed by junk doesn't
      get treated as a real token). */
  function extractTalkToken(s: string | null | undefined): string | null {
    if (!s) return null
    const m = s.match(/\/call\/([A-Za-z0-9]{4,32})\b/)
    return m ? m[1] : null
  }

  /** Like `extractTalkToken`, but returns the *full* `https://…/call/<token>`
      URL so we can hand it to `open_url`.  Used by the join-meeting
      shortcut on the location row (#209): when LOCATION already
      carries a Talk URL, the row's button switches from "Talk meeting"
      (mint a new room) to "Join meeting" (open the existing one). */
  function extractTalkUrl(s: string | null | undefined): string | null {
    if (!s) return null
    const m = s.match(/(https?:\/\/[^\s]*?\/call\/[A-Za-z0-9]{4,32})\b/)
    return m ? m[1] : null
  }

  /** Live-derived from the `location` field — non-null whenever the
      location text already contains a Talk join URL.  Drives the
      Talk meeting / Join meeting button toggle on row 5 so the user
      doesn't have to copy-paste the URL out of the location field
      to join a meeting they've already scheduled. */
  let joinMeetingUrl = $derived(extractTalkUrl(location))

  async function remove() {
    if (mode !== 'edit' || !event) return
    if (!confirm(`Delete "${event.summary || '(no title)'}"?`)) return
    deleting = true
    error = ''
    try {
      await invoke('delete_calendar_event', { eventId: event.id })

      // Best-effort tear down the associated Talk room (#172).
      // Nextcloud doesn't auto-delete rooms attached to a deleted
      // event, so without this the user accumulates dangling
      // meeting rooms in their Talk list.  We look for a `/call/`
      // token in LOCATION first (where Nimbus + NC Calendar both
      // write the join URL), falling back to URL / DESCRIPTION
      // for events created elsewhere.  A 404 (room already gone)
      // or 403 (not the owner) is normal — log + move on.
      const token =
        extractTalkToken(event.location) ??
        extractTalkToken(event.url) ??
        extractTalkToken(event.description)
      if (token) {
        const cal = calendars.find((c) => c.id === calendarId)
        if (cal) {
          try {
            await invoke('delete_talk_room', {
              ncId: cal.nextcloud_account_id,
              roomToken: token,
            })
          } catch (e) {
            console.warn('delete_talk_room (event remove cleanup) failed:', e)
          }
        }
      }

      onsaved()
      onclose()
    } catch (e) {
      error = formatError(e) || 'Failed to delete event'
    } finally {
      deleting = false
    }
  }

  function currentCalendarLabel(): string {
    const c = calendars.find((c) => c.id === calendarId)
    if (!c) return '(unknown)'
    // Surface read-only status in the static label so the user
    // sees at a glance why this event can't be deleted (#236).
    return c.read_only ? `${c.display_name} (read-only)` : c.display_name
  }

  // Diagnostic for #236 — when EventEditor opens in edit mode,
  // log the resolved calendar's read_only flag so a missing /
  // mis-parsed privilege-set surfaces in DevTools instead of
  // landing as "Delete still shows" with no signal as to why.
  // Logged once at mount; harmless on every other open.
  $effect(() => {
    if (mode !== 'edit') return
    const c = calendars.find((cc) => cc.id === calendarId)
    console.log('[event-editor] edit-mode calendar:', {
      calendarId,
      found: !!c,
      display_name: c?.display_name,
      read_only: c?.read_only,
      total_calendars: calendars.length,
      read_only_count: calendars.filter((cc) => cc.read_only).length,
    })
  })
</script>

<!-- Backdrop click closes the editor — same UX pattern as the
     new-calendar modal.  Skips when an inner popover (DateField,
     TimeField, Select, attendee suggestions) is currently open
     so the user's first outside-click closes just the popover,
     not the whole editor.  `target === currentTarget` ensures
     clicks on the modal panel itself don't trigger this. -->
<div
  class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
  role="dialog"
  aria-modal="true"
  tabindex="-1"
  onmousedown={(e) => {
    if (e.target !== e.currentTarget) return
    if (
      document.querySelector(
        '[role="listbox"], [role="dialog"][aria-label="Pick a date"]',
      )
    ) {
      return
    }
    void cancel()
  }}
>
  <div class="w-[640px] max-h-[90vh] bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl flex flex-col">
    <header class="px-5 py-3 border-b border-surface-200 dark:border-surface-700 flex items-center justify-between gap-3">
      <h2 class="text-base font-semibold shrink-0">
        {mode === 'create' ? 'New event' : 'Edit event'}
      </h2>
      <button
        class="text-surface-500 hover:text-surface-900 dark:hover:text-surface-100"
        onclick={() => void cancel()}
        aria-label="Close"
      >✕</button>
    </header>

    <!-- #236 follow-up — when the calendar is read-only we want
         the form to *look* normal (so the user can still read the
         event details) but be non-interactive (no click, no
         focus, no typing).  HTML5 `inert` does exactly that
         without any of the dimming `<fieldset disabled>` triggers
         and without the layout quirks fieldset introduces inside
         a flex column.  The Cancel/Close button lives in the
         footer outside this region so it stays clickable.
         Split the scroll container (outer div, never inert) from
         the inert wrapper (inner div) so the scrollbar drag —
         which registers as a click on the scroll container — keeps
         working even when the form's contents are locked. -->
    <div class="flex-1 overflow-y-auto p-5">
      <div
        inert={mode === 'edit' && currentCalendarReadOnly}
        class="space-y-3"
      >
      <!-- Row 1 — Title spans the row alongside the calendar
           dropdown (mockup #128).  Title gets the lion's share
           of the width; calendar picker tucks into a fixed
           240px column on the right so long calendar names
           don't squeeze the title to nothing.  In edit mode the
           calendar is read-only (moving an event between
           calendars is a separate flow). -->
      <div class="flex items-center gap-2">
        <input
          id="event-summary"
          class="input flex-1 px-3 py-2 text-sm rounded-md"
          bind:value={summary}
          placeholder="Title of the event"
          aria-label="Title"
        />
        {#if mode === 'create'}
          <div class="w-60">
            <Select
              id="event-calendar"
              ariaLabel="Calendar"
              bind:value={calendarId}
              options={writableCalendars.map((c) => ({ value: c.id, label: c.display_name }))}
              placeholder="Select calendar"
            />
          </div>
        {:else}
          <span class="w-60 px-3 py-2 text-sm text-surface-600 dark:text-surface-300 truncate" title={currentCalendarLabel()}>
            {currentCalendarLabel()}
          </span>
        {/if}
      </div>

      <!-- Row 2 — Status dropdowns: RSVP (only in edit mode and
           only when the user is themselves an attendee) and
           Show-as (always).  Mockup positions these together so
           the user can adjust both without navigating away. -->
      <div class="flex items-center gap-2">
        {#if myAttendee}
          <div class="w-44">
            <Select
              id="event-rsvp"
              ariaLabel="Your response (RSVP)"
              value={(myAttendee.status ?? 'NEEDS-ACTION').toUpperCase()}
              options={[
                { value: 'NEEDS-ACTION', label: 'No response', iconName: 'help', iconColorClass: 'text-surface-500' },
                { value: 'ACCEPTED', label: 'Accepted', iconName: 'rsvp-accept', iconColorClass: 'text-success-500' },
                { value: 'TENTATIVE', label: 'Tentative', iconName: 'rsvp-tentative', iconColorClass: 'text-warning-500' },
                { value: 'DECLINED', label: 'Declined', iconName: 'rsvp-decline', iconColorClass: 'text-error-500' },
              ]}
              onchange={(v) => {
                const target = myAttendee
                if (!target) return
                target.status = v
                requiredAttendees = [...requiredAttendees]
                optionalAttendees = [...optionalAttendees]
                chairAttendees = [...chairAttendees]
              }}
            />
          </div>
        {/if}
        <div class="w-44">
          <Select
            id="event-transp"
            ariaLabel="Show as (busy / free)"
            bind:value={transparency}
            options={[
              { value: 'OPAQUE', label: 'Busy' },
              { value: 'TRANSPARENT', label: 'Free' },
            ]}
          />
        </div>
        <div class="flex-1">
          <Select
            id="event-reminder"
            ariaLabel="Reminder"
            bind:value={reminderChoice}
            options={[
              { value: 'none', label: 'No reminder' },
              { value: '0', label: 'At event start' },
              { value: '5', label: '5 minutes before' },
              { value: '10', label: '10 minutes before' },
              { value: '15', label: '15 minutes before' },
              { value: '30', label: '30 minutes before' },
              { value: '45', label: '45 minutes before' },
              { value: '60', label: '1 hour before' },
              { value: '120', label: '2 hours before' },
              { value: '1440', label: '1 day before' },
              { value: '10080', label: '1 week before' },
              ...(reminderChoice === 'custom'
                ? [{ value: 'custom' as const, label: 'Custom (preserved from server)' }]
                : []),
            ]}
          />
        </div>
      </div>

      <!-- All-day toggle on its own thin row above the
           date/time grid so Start and End sit symmetrically
           on the row beneath it. -->
      <div class="flex items-center gap-2 text-xs text-surface-600 dark:text-surface-300">
        <Toggle
          bind:checked={allDay}
          label="All-day event"
          onchange={() => onToggleAllDay()}
        />
        All-day event
      </div>

      <!-- Symmetric Start ↔ End row.  Two equal columns, each
           with its own date + (optional) time field.  When
           `allDay` is on the time fields collapse and the
           dates take the full column width.  Custom DateField
           / TimeField components (#126) replace the native
           HTML5 inputs — the native pickers vary too much
           across platforms / browsers and don't match the
           "calendar grid + slot list" UX the issue asks
           for. -->
      <div class="grid grid-cols-2 gap-3">
        <div>
          <span class="text-xs text-surface-500 mb-1 block">Start</span>
          <div class="flex items-center gap-2">
            <div class="flex-1 min-w-0">
              <DateField
                id="event-start-date"
                ariaLabel="Start date"
                bind:value={startDate}
              />
            </div>
            {#if !allDay}
              <div class="w-28">
                <TimeField
                  id="event-start-time"
                  ariaLabel="Start time"
                  bind:value={startTime}
                />
              </div>
            {/if}
          </div>
        </div>
        <div>
          <span class="text-xs text-surface-500 mb-1 block">End</span>
          <div class="flex items-center gap-2">
            <div class="flex-1 min-w-0">
              <DateField
                id="event-end-date"
                ariaLabel="End date"
                bind:value={endDate}
              />
            </div>
            {#if !allDay}
              <div class="w-28">
                <TimeField
                  id="event-end-time"
                  ariaLabel="End time"
                  bind:value={endTime}
                />
              </div>
            {/if}
          </div>
        </div>
      </div>

      <!-- Row 5 — Location + Talk meeting shortcut.  When LOCATION
           does NOT already contain a Talk URL, the button mints a
           fresh Nextcloud Talk room and writes its URL into
           LOCATION (matches NC Calendar's "Make it a Talk
           conversation" flow); the URL replaces whatever was
           typed when the field is empty, otherwise it appends to
           DESCRIPTION (handled in `addTalkLink`).
           When LOCATION already contains a `/call/<token>` URL
           (#209) the button flips to "Join meeting" and opens the
           link via `open_url`, saving the user a copy-paste round
           trip when they're attending a meeting they (or someone
           else) scheduled. -->
      <div class="flex items-center gap-2">
        <input
          id="event-location"
          class="input flex-1 px-3 py-2 text-sm rounded-md"
          bind:value={location}
          placeholder="Location"
          aria-label="Location"
        />
        {#if joinMeetingUrl}
          <button
            type="button"
            class="btn btn-sm preset-filled-primary-500 whitespace-nowrap"
            title="Open the Talk meeting in your browser"
            onclick={() =>
              void invoke('open_url', { url: joinMeetingUrl }).catch((err) =>
                console.warn('open_url failed for Talk meeting', err),
              )}
          >
            <Icon name="open-link" size={14} class="inline-block align-text-bottom mr-1.5" />Join meeting
          </button>
        {:else}
          <button
            type="button"
            class="btn btn-sm preset-outlined-primary-500 whitespace-nowrap"
            disabled={creatingTalkRoom || !calendarId}
            title="Create a Nextcloud Talk room and use its link"
            onclick={() => void addTalkLink()}
          >
            {#if creatingTalkRoom}
              Creating…
            {:else}
              <Icon name="meetings" size={14} class="inline-block align-text-bottom mr-1.5" />Talk meeting
            {/if}
          </button>
        {/if}
      </div>

      <!-- Row 6 — Description.  Tall by default — the mockup
           shows it occupying the bulk of the form's middle. -->
      <div>
        <textarea
          id="event-description"
          class="textarea w-full px-3 py-2 text-sm rounded-md min-h-[140px]"
          bind:value={description}
          placeholder="Description"
          aria-label="Description"
        ></textarea>
      </div>

      <!-- Attendee section, split into two halves per the
           mockup (#128):
             1. Three full-width inputs at the top — one for
                each role (Hosts / Required / Optional) — so
                adding people is a clean type-and-press flow
                without chips elbowing into the input row.
             2. Below the section separator, the chip lists
                grouped under role headers, only rendered when
                there's something in the bucket so the editor
                stays compact for personal events with no
                attendees.
           Each input still drives its own debounced
           `search_contacts` dropdown with photo previews. -->

      {#snippet attendeeInput(role: Role, placeholder: string)}
        <div class="relative">
          <!-- Leading person icon — same visual vocabulary as
               the trailing icons on DateField/TimeField.  Sits
               inside the input's left padding so the placeholder
               text doesn't overlap. -->
          <svg
            xmlns="http://www.w3.org/2000/svg"
            class="w-4 h-4 text-surface-500 absolute left-3 top-1/2 -translate-y-1/2 pointer-events-none"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" />
            <circle cx="12" cy="7" r="4" />
          </svg>
          <input
            type="text"
            class="input w-full pl-9 pr-3 py-2 text-sm rounded-md"
            {placeholder}
            autocomplete="off"
            value={role === 'REQ-PARTICIPANT'
              ? requiredInput
              : role === 'OPT-PARTICIPANT'
                ? optionalInput
                : chairInput}
            oninput={(e) => {
              const v = (e.currentTarget as HTMLInputElement).value
              if (role === 'REQ-PARTICIPANT') requiredInput = v
              else if (role === 'OPT-PARTICIPANT') optionalInput = v
              else chairInput = v
              activeSuggestionRole = role
              runSuggestionSearch(role, v)
            }}
            onfocus={() => {
              activeSuggestionRole = role
              const v =
                role === 'REQ-PARTICIPANT'
                  ? requiredInput
                  : role === 'OPT-PARTICIPANT'
                    ? optionalInput
                    : chairInput
              if (v.trim().length >= 2) runSuggestionSearch(role, v)
            }}
            onblur={() => {
              setTimeout(() => {
                if (activeSuggestionRole === role) {
                  activeSuggestionRole = null
                  dropdownSuggestions = []
                }
                commitInput(role)
              }, 120)
            }}
            onkeydown={(e) => {
              const open = activeSuggestionRole === role && dropdownSuggestions.length > 0
              if (open && e.key === 'ArrowDown') {
                e.preventDefault()
                activeIndex = (activeIndex + 1) % dropdownSuggestions.length
              } else if (open && e.key === 'ArrowUp') {
                e.preventDefault()
                activeIndex =
                  (activeIndex - 1 + dropdownSuggestions.length) %
                  dropdownSuggestions.length
              } else if (open && (e.key === 'Enter' || e.key === 'Tab')) {
                e.preventDefault()
                pickSuggestion(role, dropdownSuggestions[activeIndex])
              } else if (e.key === 'Enter' || e.key === ',') {
                e.preventDefault()
                commitInput(role)
              } else if (e.key === 'Escape') {
                activeSuggestionRole = null
                dropdownSuggestions = []
              }
            }}
          />
          {#if activeSuggestionRole === role && dropdownSuggestions.length > 0}
            <ul
              class="absolute left-0 right-0 top-full mt-1 z-50 max-h-72 overflow-y-auto bg-surface-50 dark:bg-surface-900 border border-surface-300 dark:border-surface-700 rounded-md shadow-lg"
              role="listbox"
            >
              {#each dropdownSuggestions as s, i (s.kind === 'contact' ? `${s.contact.id}::${s.email.value}` : s.list.id)}
                <li
                  role="option"
                  aria-selected={i === activeIndex}
                  class="flex items-center gap-3 px-3 py-2 cursor-pointer text-sm {i === activeIndex
                    ? 'bg-primary-500/15'
                    : 'hover:bg-surface-200 dark:hover:bg-surface-800'}"
                  onmousedown={(e) => {
                    e.preventDefault()
                    pickSuggestion(role, s)
                  }}
                  onmouseenter={() => (activeIndex = i)}
                >
                  {#if s.kind === 'contact'}
                    {@const c = s.contact}
                    {@const url = photoUrl(c)}
                    {#if url}
                      <img
                        src={url}
                        alt=""
                        loading="lazy"
                        class="w-8 h-8 rounded-full object-cover flex-shrink-0"
                      />
                    {:else}
                      <div class="w-8 h-8 rounded-full bg-surface-300 dark:bg-surface-700 flex items-center justify-center text-xs font-semibold flex-shrink-0">
                        {initials(c.display_name)}
                      </div>
                    {/if}
                    <div class="flex-1 min-w-0">
                      <p class="font-medium truncate">{c.display_name}</p>
                      <p class="text-xs text-surface-500 truncate flex items-center gap-1.5">
                        <EmailKindChip kind={s.email.kind} />
                        <span class="truncate">{s.email.value}</span>
                        {#if c.organization}<span class="shrink-0">· {c.organization}</span>{/if}
                      </p>
                    </div>
                  {:else}
                    {@const list = s.list}
                    {@const icon = list.emoji || (list.source === 'team' ? '⚡' : list.source === 'manual' ? '📨' : '🏷️')}
                    {@const memberCount = list.members.filter((m) => m.email).length}
                    <div class="w-8 h-8 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-300 flex items-center justify-center text-base flex-shrink-0">
                      {icon}
                    </div>
                    <div class="flex-1 min-w-0">
                      <p class="font-medium truncate">{list.name}</p>
                      <p class="text-xs text-surface-500 truncate">
                        {memberCount} member{memberCount === 1 ? '' : 's'} · {list.source}
                      </p>
                    </div>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {/snippet}

      {#snippet chipList(label: string, role: Role, list: EventAttendee[])}
        {#if list.length > 0}
          <div>
            <div class="border-t border-surface-200 dark:border-surface-700 pt-3 mb-2">
              <span class="text-xs uppercase tracking-wide text-surface-500">{label}</span>
            </div>
            <div class="flex flex-wrap gap-2">
              {#each list as a (a.email)}
                {@const c = contactsByEmail.get(a.email.toLowerCase())}
                {@const photo = photoUrl(c)}
                {@const isOrganizerSelf = userIdentities.has(a.email.toLowerCase()) && (a.role ?? '').toUpperCase() === 'CHAIR'}
                {@const partstat = (a.status ?? '').toUpperCase()}
                {@const showRsvpBadge = !isOrganizerSelf && partstat && partstat !== 'NEEDS-ACTION'}
                {@const rsvpName = partstat === 'ACCEPTED' ? 'rsvp-accept'
                  : partstat === 'DECLINED' ? 'rsvp-decline'
                  : partstat === 'TENTATIVE' ? 'rsvp-tentative'
                  : 'help'}
                {@const rsvpColor = partstat === 'ACCEPTED' ? 'text-success-500'
                  : partstat === 'DECLINED' ? 'text-error-500'
                  : partstat === 'TENTATIVE' ? 'text-warning-500'
                  : 'text-surface-500'}
                <span class="inline-flex items-center gap-3 pl-1 pr-2 py-1 rounded-full text-sm bg-surface-200 dark:bg-surface-700 max-w-full">
                  <!-- Avatar + RSVP overlay (#179).  The
                       partstat badge sits at bottom-right of
                       the photo so a glance at the chip tells
                       you "who + how they responded" without
                       a second text label.  `ring-2` of the
                       chip's surface colour cuts the badge
                       cleanly against the avatar edge. -->
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
                        {initials(chipLabel(a))}
                      </div>
                    {/if}
                    {#if showRsvpBadge}
                      <span
                        class="absolute bottom-0 -right-2 w-4 h-4 rounded-full bg-surface-50 dark:bg-surface-900 ring-1 ring-surface-200 dark:ring-surface-700 flex items-center justify-center leading-none {rsvpColor}"
                        title={`Response: ${partstat.toLowerCase()}`}
                        aria-label={`Response: ${partstat.toLowerCase()}`}
                      >
                        <Icon name={rsvpName} size={10} class="block" />
                      </span>
                    {/if}
                  </span>
                  <span class="flex flex-col min-w-0">
                    <span class="flex items-center gap-1.5 max-w-[260px]">
                      <span class="truncate leading-tight font-medium" title={a.email}>{chipLabel(a)}</span>
                      {#if isInternal(a.email)}
                        <span
                          class="text-[9px] uppercase tracking-wide font-semibold px-1 py-px rounded bg-primary-500/20 text-primary-700 dark:text-primary-300 leading-tight shrink-0"
                          title="Nextcloud user on this server"
                        >internal</span>
                      {/if}
                    </span>
                    {#if isOrganizerSelf}
                      <!-- The user's own CHAIR row is the
                           organizer.  Override whatever PARTSTAT
                           it carries (typically auto-set to
                           ACCEPTED) with the more meaningful
                           "Organizer" label, so the user doesn't
                           see themselves as just another
                           "accepted" guest.  All other CHAIRs
                           (co-hosts) keep their response status
                           via the avatar-overlay badge above. -->
                      <span class="text-[10px] uppercase tracking-wide text-primary-600 dark:text-primary-300 leading-tight" title="You are the organizer">
                        organizer
                      </span>
                    {:else if c && c.organization}
                      <span class="text-[11px] text-surface-500 leading-tight truncate max-w-[220px]">{c.organization}</span>
                    {/if}
                  </span>
                  <button
                    type="button"
                    class="text-surface-500 hover:text-red-500 ml-1 text-base leading-none"
                    title="Remove"
                    aria-label={`Remove ${a.email}`}
                    onclick={() => removeAttendee(role, a.email)}
                  >×</button>
                </span>
              {/each}
            </div>
          </div>
        {/if}
      {/snippet}

      <!-- Three input rows, one per role.  Order matches the
           mockup: Hosts (CHAIR) → Required → Optional. -->
      {@render attendeeInput('CHAIR', 'Hosts attendees')}
      {@render attendeeInput('REQ-PARTICIPANT', 'Required attendees')}
      {@render attendeeInput('OPT-PARTICIPANT', 'Optional attendees')}

      <!-- Section separator + the chip lists grouped by role.
           Each header is faint until there's content under it,
           and `chipList` short-circuits empty buckets so the
           card collapses cleanly for unattended events. -->
      {@render chipList('Hosts', 'CHAIR', chairAttendees)}
      {@render chipList('Required', 'REQ-PARTICIPANT', requiredAttendees)}
      {@render chipList('Optional', 'OPT-PARTICIPANT', optionalAttendees)}

      {#if error}
        <p class="text-sm text-red-500">{error}</p>
      {/if}
      </div>
    </div>

    {#if mode === 'edit' && currentCalendarReadOnly}
      <!-- Read-only banner (#236 follow-up).  Sits just above
           the footer so the user sees *why* Save / Delete
           aren't on offer before reaching for them.  The
           banner appears either because the calendar's
           privilege-set was parsed at discovery OR because a
           previous Save attempt got back a 403/404 and the
           write-failure fallback flipped the flag. -->
      <div class="px-5 py-2 border-t border-surface-200 dark:border-surface-700 text-xs text-surface-600 dark:text-surface-300 flex items-center gap-2">
        <Icon name="warning" size={14} />
        <span>This calendar is read-only — you can view this event, but changes can't be saved.</span>
      </div>
    {/if}

    <footer class="px-5 py-3 border-t border-surface-200 dark:border-surface-700 flex items-center gap-2">
      {#if !(mode === 'edit' && currentCalendarReadOnly)}
        <!-- #236: hide Save when the calendar is read-only.
             The server would reject the PUT (NC's permission-
             masking returns 404 instead of 403, our CalDAV
             write path catches both); offering the button
             just produces a confusing error toast.  Create-
             mode never lands here because the calendar
             picker filters read-only entries out upstream. -->
        <button class="btn preset-filled-primary-500" disabled={saving || deleting} onclick={save}>
          {saving
            ? 'Saving…'
            : mode === 'create'
              ? 'Create'
              : mode === 'stage'
                ? 'Add to message'
                : 'Save'}
        </button>
      {/if}
      {#if mode === 'edit' && !currentCalendarReadOnly}
        <!-- #236: hide Delete when the calendar is read-only.
             A user can still open such an event to view the
             details, but the server would reject the
             `DELETE` so showing the button would just produce
             a confusing error toast. -->
        <button class="btn preset-outlined-error-500" disabled={saving || deleting} onclick={remove}>
          {deleting ? 'Deleting…' : 'Delete'}
        </button>
      {/if}
      <div class="flex-1"></div>
      <button class="btn preset-outlined-surface-500" disabled={saving || deleting} onclick={() => void cancel()}>
        {mode === 'edit' && currentCalendarReadOnly ? 'Close' : 'Cancel'}
      </button>
    </footer>
  </div>
</div>
