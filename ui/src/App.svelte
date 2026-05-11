<script lang="ts">
  /**
   * App.svelte — root component and simple view router.
   *
   * On startup, it asks the Rust backend how many accounts exist:
   *   - 0 accounts → show the AccountSetup wizard
   *   - 1+ accounts → show the main inbox (3-panel layout)
   *
   * The user can also navigate to AccountSettings from the sidebar.
   * This is a simple state-based "router" — no URL routing needed
   * since this is a desktop app, not a website.
   */

  import { invoke } from '@tauri-apps/api/core'
  import { listen, type UnlistenFn } from '@tauri-apps/api/event'
  import {
    isPermissionGranted,
    requestPermission,
    sendNotification,
  } from '@tauri-apps/plugin-notification'
  import Icon from './lib/Icon.svelte'
  import IconRail, { type RailView } from './lib/IconRail.svelte'
  import Sidebar from './lib/Sidebar.svelte'
  import MailList from './lib/MailList.svelte'
  import MailView from './lib/MailView.svelte'
  import AccountSetup from './lib/AccountSetup.svelte'
  import AccountSettings from './lib/AccountSettings.svelte'
  import LockScreen from './lib/LockScreen.svelte'
  import Compose, {
    type ComposeInitial,
    type SendFailurePayload,
  } from './lib/Compose.svelte'
  import ContactsView from './lib/ContactsView.svelte'
  import CalendarView from './lib/CalendarView.svelte'
  import { openReminderInStandaloneWindow } from './lib/reminderPopupWindow'
  import { openMailFileInStandaloneWindow } from './lib/standaloneMailFileWindow'
  import OutboxList, { type OutboxRowDto } from './lib/OutboxList.svelte'
  import OutboxView from './lib/OutboxView.svelte'
  import FilesView from './lib/FilesView.svelte'
  import TalkView from './lib/TalkView.svelte'
  import NotesView from './lib/NotesView.svelte'
  import EventEditor, { type SavedEvent } from './lib/EventEditor.svelte'
  import { quotedHistoryHtml, type MeetingInvite } from './lib/inviteHtml'
  import SearchBar, {
    type SearchScope,
    type SearchFilters,
  } from './lib/SearchBar.svelte'
  import SearchResults from './lib/SearchResults.svelte'
  import {
    applyTheme,
    installSystemModeListener,
    registerCustomThemePath,
    setCustomThemes,
    unregisterCustomThemePath,
    type ThemeMode,
    type ThemeOption,
  } from './lib/theme'
  import {
    applyUiScale,
    clampScale,
    effectiveScale,
    UI_SCALE_STEP,
  } from './lib/uiScale'
  import { locales } from './paraglide/runtime'

  // ── View state ──────────────────────────────────────────────
  // Which view is currently shown. Starts as 'loading' until we
  // check whether any accounts exist.
  type View =
    | 'loading'
    | 'setup'
    | 'inbox'
    | 'settings'
    | 'contacts'
    | 'calendar'
    | 'files'
    | 'talk'
    | 'notes'
  let currentView = $state<View>('loading')


  // ── Inbox state ─────────────────────────────────────────────
  // All configured mail accounts and which one the user is currently
  // looking at. Kept at the App level so Sidebar / MailList / MailView
  // stay in sync when the user switches accounts. `activeAccountEmail`
  // is derived from the list so it stays consistent if an account's
  // email is edited in settings.
  interface Account {
    id: string
    display_name: string
    email: string
    /** User-defined folder icon rules. The Sidebar reads this off
        the active account to apply per-account theming. Optional
        because older `accounts.json` files predate the field. */
    folder_icons?: { keyword: string; icon: string }[]
    /** Per-folder icon overrides (full path → emoji). Set via the
        Sidebar's right-click → Change icon picker; wins over
        special-use / keyword rules in `folderIcon`. Optional for
        the same back-compat reason as `folder_icons`. */
    folder_icon_overrides?: Record<string, string>
    /** Display order in the IconRail; lower = top.  Lets us pick
     *  the visually-first account on launch instead of the one
     *  that happens to be first in the DB's insertion order. */
    sort_order?: number
  }
  let accounts = $state<Account[]>([])
  let activeAccountId = $state<string | null>(null)

  // ── Nextcloud capability snapshot (#189) ────────────────────
  // Drives which integration icons (Contacts / Calendar / Files /
  // Talk / Notes) the IconRail surfaces at all.  Aggregated from
  // every connected NC account: a feature is "available" if any
  // one account exposes it.  Refreshed on the same `loadAppPrefs`
  // tick that hydrates the rest of the shell so a user adding a
  // Nextcloud account in Settings sees the rail update on close.
  interface NextcloudAccountWithCaps {
    id: string
    capabilities?: {
      talk?: boolean
      files?: boolean
      caldav?: boolean
      carddav?: boolean
      notes?: boolean
    } | null
  }
  let ncCaps = $state({
    /** True when at least one NC account is connected — gates
     *  every integration icon at once.  Without this, a user
     *  who hasn't added a Nextcloud account yet would still see
     *  the integration nav, click into one, and hit a "no
     *  accounts" empty state. */
    hasAny: false,
    contacts: false,
    calendar: false,
    files: false,
    talk: false,
    notes: false,
  })

  async function refreshNextcloudCapabilities() {
    try {
      const list = await invoke<NextcloudAccountWithCaps[]>('get_nextcloud_accounts')
      const any = (pred: (a: NextcloudAccountWithCaps) => boolean) =>
        list.some(pred)
      ncCaps = {
        hasAny: list.length > 0,
        contacts: any((a) => a.capabilities?.carddav === true),
        calendar: any((a) => a.capabilities?.caldav === true),
        files: any((a) => a.capabilities?.files === true),
        talk: any((a) => a.capabilities?.talk === true),
        notes: any((a) => a.capabilities?.notes === true),
      }
    } catch (e) {
      console.warn('refreshNextcloudCapabilities failed', e)
      // Leave the previous snapshot in place on failure — better
      // to keep a stale "available" answer than to flicker icons
      // off because of a transient IPC error.
    }
  }

  // ── Database lock state (#164 Phase 1B) ─────────────────────
  // Cache may be in FIDO-only mode at boot; in that case the
  // lock screen mounts ahead of every other view and the rest
  // of the app stays inert until the user authenticates.
  interface DatabaseStatus {
    locked: boolean
    needsSetup: boolean
    methods: {
      kind: 'fido_prf' | 'passphrase'
      credentialId: string
      label: string
      salt: string
      createdAt: number
    }[]
    attemptsRemaining: number | null
  }
  let dbStatus = $state<DatabaseStatus | null>(null)
  let dbStatusError = $state('')
  $effect(() => {
    void invoke<DatabaseStatus>('database_status')
      .then((s) => (dbStatus = s))
      .catch((e) => {
        console.warn('database_status failed', e)
        dbStatusError = String(e)
        // Fail-open: assume unlocked so the user isn't trapped on
        // a blank screen if the IPC went wrong.  Real lock-state
        // bugs surface as "every other IPC errors with locked".
        dbStatus = { locked: false, needsSetup: false, methods: [], attemptsRemaining: null }
      })
  })
  function onUnlocked() {
    if (dbStatus) dbStatus = { ...dbStatus, locked: false }
  }
  const activeAccountEmail = $derived(
    accounts.find((a) => a.id === activeAccountId)?.email ?? '',
  )
  // Unified-inbox mode: when on, MailList aggregates INBOX across all
  // accounts. `activeAccountId` stays pointed at a real account so the
  // sidebar's folder tree, integrations, and default Compose-from
  // continue to have a sensible "current account" — the unified view
  // is an overlay on top, not a replacement for the active account.
  let unifiedMode = $state(false)
  // The account a clicked message belongs to. In single-account mode
  // this just shadows `activeAccountId`; in unified mode it's set from
  // the row's `account_id` so MailView opens the right message even
  // though the folder picker isn't pointing at that account.
  let selectedMessageAccountId = $state<string | null>(null)
  // Compose modal: `null` = closed. When open, carries a (possibly empty)
  // initial prefill for reply / reply-all / forward.
  let composeInitial = $state<ComposeInitial | null>(null)
  // Default to INBOX — the Sidebar replaces this as soon as the user
  // picks a folder, or could switch it automatically if INBOX is absent.
  let selectedFolder = $state<string>('INBOX')
  let selectedUid = $state<number | null>(null)
  // Bumped to force child lists to re-fetch (manual refresh, mark-as-read).
  let refreshToken = $state(0)

  /** Synthetic local-only Outbox folder name (#276).  Mirror of
   *  the constant in Sidebar.svelte; same name means selecting
   *  the synthetic sidebar entry routes through the existing
   *  `selectedFolder` channel and the template here can branch
   *  on a string compare. */
  const OUTBOX_FOLDER = 'Outbox'

  /** Total queued rows in the local Outbox table (#276).  Drives
   *  the Sidebar's "render the synthetic Outbox folder?"
   *  decision.  Refreshed via the `outbox-updated` Tauri event
   *  the backend fires whenever the queue changes shape. */
  let outboxCount = $state(0)
  /** Per-component re-fetch nonce for OutboxList — bumped on
   *  `outbox-updated` so the list reloads even when no other
   *  state change would have triggered its `$effect`. */
  let outboxRefreshToken = $state(0)
  /** Currently-previewed Outbox row (#276 follow-up).  Carries
   *  the full row snapshot so the right-pane `OutboxView` can
   *  render straight from it without a second round-trip to
   *  the backend; falls back to the latest `list_outbox`
   *  response if the row is updated by a retry / failure
   *  recording while the user has it open. */
  let selectedOutboxRow = $state<OutboxRowDto | null>(null)
  /** Snapshot of "did the currently-open Compose start as an
   *  edit-from-outbox?".  Captured at `openCompose` time
   *  because Compose's `onclose` fires immediately on Send
   *  (#156's instant-close) before its async send pipeline
   *  reaches `invoke('send_email')` — so by the time we'd want
   *  to inspect `composeInitial.outboxSource`, the modal has
   *  already cleared it.  Set on open, consumed by
   *  `onsentenqueued`, cleared by a fresh `openCompose` of any
   *  kind so a subsequent non-edit send can't accidentally
   *  inherit it. */
  let composeOpenedAsEditOfOutbox = $state(false)

  // Bindable mirror of MailList's currently-rendered envelope rows.
  // Used by the auto-advance-after-delete flow (#99) to pick the
  // UID of the row directly below the one we just removed without
  // having to re-implement the cache fetch up here.  Shape mirrors
  // MailList's local `EmailEnvelope` interface — we only ever read
  // `uid` / `account_id` here, but the bind requires the full
  // shape to type-check both sides.
  type MailListEnvelope = {
    uid: number
    folder: string
    from: string
    subject: string
    date: string
    is_read: boolean
    is_starred: boolean
    account_id: string
  }
  let mailListEnvelopes = $state<MailListEnvelope[]>([])

  // Network-refresh state for the IconRail's active-account
  // avatar spinner (#161).  Each child component (MailList,
  // MailView) flips its own flag while a post-cache fetch is
  // in flight; we OR them so a refresh in either pane lights
  // the same indicator.
  let mailListRefreshing = $state(false)
  let mailViewRefreshing = $state(false)
  // Set true while an IconRail-click-driven `check_mail_now`
  // poll is in flight (#196 follow-up). The MailList /
  // MailView flags only flip when those panes do their own
  // fetch; clicking the *already-active* account avatar
  // doesn't re-mount them, so without this flag the spinner
  // wouldn't show on a re-click. Cleared in selectAccount's
  // `.finally`.
  let accountClickRefreshing = $state(false)
  const mailRefreshing = $derived(
    mailListRefreshing || mailViewRefreshing || accountClickRefreshing,
  )

  // ── Global right-click menu (#198) ───────────────────────────
  // The webview's default right-click menu (Reload / Inspect /
  // View source) shows on every surface that doesn't bind its
  // own `oncontextmenu` — confusing in a desktop app where the
  // user expects native-feeling actions.  Suppress that menu
  // app-wide and offer a small "Check mail now" fallback so a
  // right-click on inert space (toolbar, header, empty mail
  // list area) still has *some* utility.
  //
  // Custom row-level menus (mails, folders, calendars,
  // contacts) call e.preventDefault() inside their own handlers
  // before the event bubbles up here, so we read
  // `defaultPrevented` to detect "a row already claimed this"
  // and skip showing the fallback in that case.
  let appContextMenu = $state<{ x: number; y: number } | null>(null)

  $effect(() => {
    function onCtx(e: MouseEvent) {
      const customHandled = e.defaultPrevented
      e.preventDefault()
      if (customHandled) return
      appContextMenu = { x: e.clientX, y: e.clientY }
    }
    document.addEventListener('contextmenu', onCtx)
    return () => document.removeEventListener('contextmenu', onCtx)
  })

  /** Ctrl+wheel UI scale (#191).
   *  Captures the wheel event before the webview's native zoom
   *  kicks in.  Each tick adjusts by `UI_SCALE_STEP` (5 %) up
   *  or down within `[MIN_UI_SCALE, MAX_UI_SCALE]`.  The change
   *  is persisted via `set_app_settings` and flips
   *  `ui_scale_auto` off so the auto-derivation doesn't undo
   *  the user's choice on the next launch.  Persistence is
   *  fire-and-forget — a save failure leaves the in-memory
   *  scale applied and the user can retry by scrolling again. */
  $effect(() => {
    function onWheel(e: WheelEvent) {
      if (!e.ctrlKey) return
      e.preventDefault()
      if (!appPrefs) return
      const current = effectiveScale(appPrefs.ui_scale, appPrefs.ui_scale_auto)
      // wheel-up (deltaY < 0) zooms in; wheel-down zooms out.
      const direction = e.deltaY < 0 ? +1 : -1
      const next = clampScale(current + direction * UI_SCALE_STEP)
      if (next === current) return
      // Optimistic local apply so the user sees the change
      // instantly; the persist roundtrip catches up.
      applyUiScale(next)
      const updated: AppPrefs = {
        ...appPrefs,
        ui_scale: next,
        ui_scale_auto: false,
      }
      appPrefs = updated
      void invoke('set_app_settings', { settings: updated }).catch((err) =>
        console.warn('set_app_settings (ui_scale) failed', err),
      )
    }
    // `passive: false` so `preventDefault` is allowed — without
    // it the browser would also apply its own zoom on top of
    // ours, doubling the effect.
    window.addEventListener('wheel', onWheel, { passive: false })
    return () => window.removeEventListener('wheel', onWheel)
  })

  function closeAppContextMenu() {
    appContextMenu = null
  }

  /** Right-click → Refresh: kicks off a server poll on the
   *  Rust side (so new mail lands in the SQLite cache) and then
   *  reboots the Vite-hosted Svelte UI (so every component
   *  re-mounts and the freshly-cached envelopes paint). The
   *  `check_mail_now` invoke is fire-and-forget on purpose — its
   *  Promise dies with the page reload, but the *backend* command
   *  it triggered keeps running on Tauri's async runtime,
   *  unaffected by the frontend lifecycle. The reloaded UI's
   *  event listeners pick up the resulting `new-mail` /
   *  `unread-count-updated` emissions when the poll finishes. */
  function reloadFrontendFromContextMenu() {
    closeAppContextMenu()
    void invoke('check_mail_now').catch((e) => {
      console.warn('check_mail_now during refresh failed', e)
    })
    window.location.reload()
  }

  // ── Search state ────────────────────────────────────────────
  // `searchQuery` drives the mail-list column: non-empty query OR
  // any active filter swaps MailList out for SearchResults.
  let searchQuery = $state('')
  let searchScope = $state<SearchScope>({})
  let searchFilters = $state<SearchFilters>({})
  // Derived: are we in "search mode"?
  const searchActive = $derived(
    searchQuery.trim().length > 0 ||
      !!searchFilters.unreadOnly ||
      !!searchFilters.flaggedOnly ||
      !!searchFilters.hasAttachment,
  )

  function onSearch(q: string, scope: SearchScope, filters: SearchFilters) {
    searchQuery = q
    searchScope = scope
    searchFilters = filters
  }

  // When a search hit is picked, follow its folder — the hit may
  // live in a different folder than the one currently selected
  // (e.g. "All folders" scope). Syncing the folder makes the
  // subsequent MailView fetch + sidebar highlight coherent.
  function onSelectSearchHit(uid: number, folder: string) {
    if (folder !== selectedFolder) {
      selectedFolder = folder
    }
    selectedUid = uid
  }

  // ── Check for existing accounts on startup ──────────────────
  // Wait until the cache is actually unlocked before asking Rust
  // for the account list — `get_accounts` returns `Locked` while
  // the FIDO unlock screen is up, and that error path used to
  // route the user into the setup wizard even when accounts
  // existed.  Re-runs after `onUnlocked` flips `dbStatus.locked`.
  $effect(() => {
    if (dbStatus && !dbStatus.locked) {
      void checkAccounts()
    }
  })

  // ── Issue #16: background-sync events + desktop notifications ──
  //
  // Rust emits a `new-mail` event per newly-fetched envelope and an
  // `unread-count-updated` event after each poll cycle. The frontend
  // owns notification display so there's a single permission check
  // path and a single formatting path.
  //
  // Notification burst cap: if more than 3 `new-mail` events land
  // inside a 2-second window, the tail gets collapsed into one
  // summary toast — avoids a rain of toasts after a long offline
  // period or on first JMAP sync.
  let notificationsGranted = $state(false)
  // Absolute path to our app icon, fetched once at startup. Passed
  // to `sendNotification` so libnotify (Linux) / NSUserNotification
  // (macOS) / WinRT (Windows) show the Nimbus icon next to each
  // toast instead of a generic placeholder. Empty until the
  // backend `get_notification_icon_path` resolves.
  let notificationIconPath = $state<string>('')
  let recentBurst: number[] = []
  let pendingSummaryTimer: ReturnType<typeof setTimeout> | null = null

  type NewMail = {
    account_id: string
    folder: string
    uid: number
    from: string
    subject: string
  }

  /** `mail-flags-updated` event payload (#255 follow-up).  Backend
   *  fires this whenever the cached `\Seen` / `\Flagged` /
   *  `\Answered` flags change — either Compose's post-send
   *  marking or the poll path's cross-client catch-up.  We don't
   *  inspect the fields today (the listener just bumps
   *  `refreshToken` to re-read the cache), but they're kept on
   *  the type so per-folder filtering stays an easy follow-up. */
  type MailFlagsUpdatedPayload = {
    accountId: string
    folder: string
  }

  type AppPrefs = {
    minimize_to_tray: boolean
    background_sync_enabled: boolean
    background_sync_interval_secs: number
    notifications_enabled: boolean
    start_minimized: boolean
    theme_name: string
    theme_mode: ThemeMode
    mail_html_white_background: boolean
    auto_load_remote_images: boolean
    auto_advance_after_remove: boolean
    /** #203: gates reminders for events that carry a meeting URL. */
    meeting_reminders_enabled: boolean
    /** #203: gates reminders for events without a meeting URL. */
    calendar_reminders_enabled: boolean
    /** #203 follow-up: when true, a meeting reminder firing at
     *  "now" (≤1 min lead) opens the meeting URL in the
     *  browser straight away instead of showing the popup.
     *  Off by default. */
    auto_open_meetings: boolean
    autostart_enabled: boolean
    /** User-imported Skeleton themes (#132 tier 2). */
    custom_themes?: CustomTheme[]
    /** #191: manual UI-scale multiplier applied via CSS `zoom`
     *  on the document root.  Range 0.7 .. 1.5. */
    ui_scale?: number
    /** #191: when true, `ui_scale` is ignored and the
     *  effective scale is derived from screen width on
     *  every launch.  User actions that pick an explicit
     *  scale (slider, Ctrl+wheel) flip this to false. */
    ui_scale_auto?: boolean
    /** #190: BCP-47 locale tag for the display language.  Empty
     *  means "follow `navigator.language`".  Currently only
     *  `"en"` and `"de"` are populated. */
    ui_locale?: string
    /** #190: when true, `ui_locale` is ignored and paraglide
     *  picks from `navigator.language` on launch. */
    ui_locale_auto?: boolean
    /** #165 master toggle for the URLhaus link checker.  When
     *  true, every link in a rendered email gets a green / red
     *  safety pill and unsafe clicks confirm.  When false,
     *  links open without interception. */
    link_check_enabled?: boolean
  }
  type CustomTheme = {
    id: string
    label: string
    description?: string
    path: string
  }

  // Issues #123 + #203: event reminders.  Rust scans upcoming
  // events on every sync tick and emits an `event-reminder`
  // event whenever any VALARM lead time elapses.  Two settings
  // gate this — `meeting_reminders_enabled` for events with
  // a meeting URL, `calendar_reminders_enabled` for everything
  // else (the user can mute one stream without the other).
  type EventReminder = {
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

  // Cached settings snapshot — refreshed when the settings command is
  // called, and consulted when a `new-mail` event arrives to decide
  // whether to show a toast.
  let appPrefs = $state<AppPrefs | null>(null)

  async function bootstrapNotifications() {
    try {
      const granted = await isPermissionGranted()
      if (granted) {
        notificationsGranted = true
        return
      }
      // Only prompt once the user is past setup — on the very first
      // launch the setup wizard should own the screen, not an OS
      // permission dialog.
      if (currentView === 'setup') return
      const res = await requestPermission()
      notificationsGranted = res === 'granted'
    } catch (err) {
      console.warn('notification permission bootstrap failed', err)
    }
  }

  /** Best-effort startup cleanup for the Office viewer's temp area
   *  on every connected Nextcloud. If Nimbus crashed mid-edit, or
   *  `office_close_attachment` errored on the way out last session,
   *  the user's `/Nimbus Mail/temp` folder accumulates orphan
   *  uploads. The Rust sweeper scopes by mtime so a still-open
   *  edit window in a parallel Nimbus instance doesn't get its
   *  file pulled out from under it. Failures are logged and
   *  swallowed — no toast, no UI block. */
  async function sweepNextcloudTempFiles() {
    try {
      const accounts = await invoke<{ id: string }[]>('get_nextcloud_accounts')
      for (const a of accounts) {
        try {
          await invoke('office_sweep_temp', { ncId: a.id })
        } catch (e) {
          console.warn('office_sweep_temp failed for', a.id, e)
        }
      }
    } catch (e) {
      console.warn('sweepNextcloudTempFiles: get_nextcloud_accounts failed', e)
    }
  }

  function shouldNotify(): boolean {
    return (
      notificationsGranted && (appPrefs?.notifications_enabled ?? true)
    )
  }

  async function fireToast(title: string, body: string) {
    // On Linux the native command sends through `notify-rust` with
    // the `DesktopEntry` hint set, so notifications land in the
    // notification center / history (GNOME Shell, KDE Plasma).  The
    // command returns `false` on non-Linux platforms so we fall
    // through to the Tauri plugin, whose macOS / Windows backends
    // already wire in the right OS hooks.
    try {
      const handled = await invoke<boolean>('send_native_notification', {
        title,
        body,
      })
      if (handled) return
    } catch (err) {
      console.warn('send_native_notification failed, falling back to plugin', err)
    }
    try {
      sendNotification({
        title,
        body,
        ...(notificationIconPath ? { icon: notificationIconPath } : {}),
      })
    } catch (err) {
      console.warn('sendNotification failed', err)
    }
  }

  /** Format "in 5 min" / "in 1 hour" / "now" given a positive
   *  lead-time in minutes — wording the body of a Talk reminder
   *  toast so the user knows how soon to drop into the call. */
  function formatLeadTime(min: number): string {
    if (min <= 0) return 'now'
    if (min < 60) return `in ${min} min`
    const hours = Math.floor(min / 60)
    const remainder = min % 60
    if (remainder === 0) return `in ${hours} hour${hours === 1 ? '' : 's'}`
    return `in ${hours}h ${remainder}m`
  }

  /** First-three-attendees + "+N more" tail for the OS toast
   *  body.  Linux libnotify wraps badly past three lines, so
   *  the OS surface gets the short version; the popout
   *  reminder window renders the same shape with more room. */
  function formatAttendees(list: string[]): string {
    if (list.length === 0) return ''
    const first = list.slice(0, 3).join(', ')
    return list.length > 3 ? `${first} +${list.length - 3} more` : first
  }

  /** Event id the user clicked "Show event" for — passed
   *  through to CalendarView so it opens that event's editor.
   *  CalendarView clears it via `oneventfocused`.  Set by the
   *  `reminder-show-event` listener below when a popup window
   *  asks the main window to surface its event. */
  let calendarFocusEventId = $state<string | null>(null)

  function handleEventReminder(payload: EventReminder) {
    // Per-event gate, mirroring the backend logic — the backend
    // already gates by these flags, but settings can change
    // between the scan and the emit, so re-check here.
    const isMeeting = !!payload.meetingUrl
    const allowed = isMeeting
      ? appPrefs?.meeting_reminders_enabled ?? true
      : appPrefs?.calendar_reminders_enabled ?? true
    if (!allowed) return

    // OS notification fires alongside so the user sees the
    // reminder even if they immediately close the popup or
    // miss it.  The popup carries the rich detail + action
    // buttons; the OS toast is a quick at-a-glance.
    if (shouldNotify()) {
      const lead = formatLeadTime(payload.minutesBefore)
      const startLocal = new Date(payload.start).toLocaleTimeString(undefined, {
        hour: '2-digit',
        minute: '2-digit',
      })
      const title = `📅 ${payload.summary || 'Event'} — ${lead}`
      const bodyLines: string[] = [`Starts at ${startLocal}`]
      if (payload.location) bodyLines.push(`📍 ${payload.location}`)
      if (payload.attendees.length > 0)
        bodyLines.push(`👥 ${formatAttendees(payload.attendees)}`)
      void fireToast(title, bodyLines.join('\n'))
    }

    // Auto-open shortcut (opt-in via Settings → Calendar):
    // for an *imminent* meeting (≤1 min lead — typically the
    // "At event start" preset on the snooze dropdown) the URL
    // opens straight away in the user's browser instead of
    // surfacing the popup.  Off by default — the popup is the
    // consistent default surface, and users who want the old
    // auto-join shortcut explicitly opt in.
    if (
      isMeeting &&
      payload.minutesBefore <= 1 &&
      payload.meetingUrl &&
      appPrefs?.auto_open_meetings
    ) {
      void invoke('open_url', { url: payload.meetingUrl }).catch((err) =>
        console.warn('open_url for event reminder failed', err),
      )
      void invoke('dismiss_event_reminder', { uid: payload.uid }).catch(
        () => {},
      )
      return
    }

    // Standalone popup — same Vite bundle, mounts
    // StandaloneReminder.svelte.  The popup carries Show event
    // / Join meeting / Snooze / Dismiss actions and survives
    // the main window being hidden in the tray.
    void openReminderInStandaloneWindow(payload).catch((err) =>
      console.warn('openReminderInStandaloneWindow failed', err),
    )
  }

  function handleNewMail(payload: NewMail) {
    // Refresh the list regardless of notification state — new mail
    // should appear in the inbox even if toasts are off.
    refreshToken++

    if (!shouldNotify()) return

    const now = Date.now()
    // Prune burst entries older than 2s — a pure sliding window.
    recentBurst = recentBurst.filter((t) => now - t < 2000)
    recentBurst.push(now)

    if (recentBurst.length <= 3) {
      void fireToast(payload.from || 'New mail', payload.subject || '(no subject)')
      return
    }

    // 4th+ toast in the window — suppress individual toast and
    // schedule one summary toast for the end of the window.
    if (pendingSummaryTimer) clearTimeout(pendingSummaryTimer)
    const count = recentBurst.length
    pendingSummaryTimer = setTimeout(() => {
      void fireToast('Nimbus Mail', `${count} new messages`)
      pendingSummaryTimer = null
    }, 600)
  }

  async function loadAppPrefs() {
    // Refresh the Nextcloud capability snapshot in parallel with
    // the settings load (#189).  Doesn't depend on settings — it
    // queries `get_nextcloud_accounts` which has its own state —
    // so kicking it off as a side-fire keeps the IconRail's
    // available-feature flags in sync after Settings closes,
    // without slowing the main settings round-trip.
    void refreshNextcloudCapabilities()

    try {
      appPrefs = await invoke<AppPrefs>('get_app_settings')
      // Seed the theme module's custom-theme registry so the
      // picker + the runtime <link> swap know about the user's
      // imported themes (#132).  Re-runs on every reload so
      // imports/removals from another window stay in sync.
      const list: CustomTheme[] = appPrefs.custom_themes ?? []
      const options: ThemeOption[] = list.map((t) => ({
        id: t.id,
        label: t.label,
        description: t.description ?? 'Imported theme',
        custom: true,
      }))
      setCustomThemes(options)
      for (const t of list) registerCustomThemePath(t.id, t.path)
      // Drop any stale entries from a previous load that the
      // user has since removed.
      const liveIds = new Set(list.map((t) => t.id))
      for (const id of Object.keys(prevCustomThemeIds)) {
        if (!liveIds.has(id)) unregisterCustomThemePath(id)
      }
      prevCustomThemeIds = Object.fromEntries(list.map((t) => [t.id, true]))
      // Apply the chosen UI scale (#191).  Resolved every time
      // `get_app_settings` returns — covers the launch path AND
      // the post-Settings-save path.  `effectiveScale` honours
      // the auto/manual toggle.
      applyUiScale(
        effectiveScale(appPrefs.ui_scale, appPrefs.ui_scale_auto),
      )
      // Sync the paraglide localStorage pin with the persisted
      // settings (#190).  Paraglide reads localStorage at boot
      // and never re-checks; the actual UI language for *this*
      // process was therefore already decided by the time the
      // settings come back from disk.  We just make sure the
      // pin matches what's saved so a future restart picks up
      // the right value.  The Settings page's language picker
      // owns the in-process restart prompt — see the comment
      // there.
      try {
        if (appPrefs.ui_locale_auto !== false) {
          window.localStorage.removeItem('PARAGLIDE_LOCALE')
        } else if (
          appPrefs.ui_locale &&
          (locales as readonly string[]).includes(appPrefs.ui_locale)
        ) {
          window.localStorage.setItem('PARAGLIDE_LOCALE', appPrefs.ui_locale)
        }
      } catch {}
    } catch (err) {
      console.warn('get_app_settings failed', err)
    }
  }
  let prevCustomThemeIds: Record<string, boolean> = {}

  async function loadNotificationIconPath() {
    try {
      notificationIconPath = await invoke<string>('get_notification_icon_path')
    } catch (err) {
      console.warn('get_notification_icon_path failed', err)
    }
  }

  /** Re-apply the theme + (re)install the OS-mode listener whenever
      the user's theme preferences change. The effect's cleanup
      function tears down the previous listener before the next run
      installs a new one, so we never leak `matchMedia` subscribers
      when the user toggles between System / Light / Dark. */
  $effect(() => {
    if (!appPrefs) return
    applyTheme(appPrefs.theme_name, appPrefs.theme_mode)
    return installSystemModeListener(appPrefs.theme_mode, appPrefs.theme_name)
  })

  $effect(() => {
    loadAppPrefs()
    bootstrapNotifications()
    void loadNotificationIconPath()
    void sweepNextcloudTempFiles()

    let unlistenNewMail: UnlistenFn | null = null
    let unlistenEventReminder: UnlistenFn | null = null
    let unlistenReminderShowEvent: UnlistenFn | null = null
    let unlistenCustomThemes: UnlistenFn | null = null
    let unlistenCompose: UnlistenFn | null = null
    let unlistenComposeFromMail: UnlistenFn | null = null
    let unlistenEditDraftFromMail: UnlistenFn | null = null
    let unlistenMailtoFromMail: UnlistenFn | null = null
    let unlistenMailFlagsUpdated: UnlistenFn | null = null
    let unlistenOutboxUpdated: UnlistenFn | null = null
    ;(async () => {
      unlistenNewMail = await listen<NewMail>('new-mail', (e) =>
        handleNewMail(e.payload),
      )
      // #255: backend fires this whenever the answered / read /
      // starred flag on a cached envelope changes (Compose's
      // post-send marking, plus the cross-client catch-up the
      // poll path runs).  Bump the refresh token so MailList
      // re-reads the cache and the row picks up the new flags
      // without a manual refresh.
      unlistenMailFlagsUpdated = await listen<MailFlagsUpdatedPayload>(
        'mail-flags-updated',
        () => {
          refreshToken++
        },
      )
      // #276: backend fires `outbox-updated` whenever the queue
      // changes shape (enqueue / drain success / failure /
      // delete).  Refresh the count so the Sidebar shows /
      // hides the synthetic Outbox folder, and bump the
      // OutboxList's nonce so its rows re-fetch.
      unlistenOutboxUpdated = await listen<{ total: number }>(
        'outbox-updated',
        (e) => {
          outboxCount = Math.max(0, Math.floor(e.payload.total ?? 0))
          outboxRefreshToken++
          // If the user is sitting on the Outbox folder and the
          // queue just drained empty, route them back to INBOX
          // — staying on an empty Outbox would just be a blank
          // surface they have to manually navigate away from.
          if (outboxCount === 0 && selectedFolder === OUTBOX_FOLDER) {
            selectedFolder = 'INBOX'
            selectedUid = null
            selectedOutboxRow = null
          }
          // OutboxList itself re-runs `list_outbox` on this same
          // event and emits `onselect(updatedRow | null)` —
          // that's where the preview row gets refreshed (so
          // attempt counts / last_error stay fresh) or cleared
          // (when the row drained or got deleted).
        },
      )
      // Seed the count once on startup so a queue carried over
      // from a previous session is reflected without waiting for
      // the first poll tick.
      try {
        outboxCount = await invoke<number>('count_outbox')
      } catch (e) {
        console.warn('count_outbox at startup failed', e)
      }
      unlistenEventReminder = await listen<EventReminder>(
        'event-reminder',
        (e) => handleEventReminder(e.payload),
      )
      // The reminder popup window emits this when the user
      // clicks "Show event".  We flip to the calendar view and
      // thread the event id into CalendarView so it opens the
      // editor.  The popup also calls `show_main_window_cmd`
      // (Rust IPC) to actually bring this window to the
      // foreground — JS-side `setFocus()` from a non-foreground
      // window is unreliable on Windows because of the
      // `SetForegroundWindow` lock, especially when the main
      // window is hidden in the system tray.  Doing it from
      // Rust avoids that.
      unlistenReminderShowEvent = await listen<{ eventId: string }>(
        'reminder-show-event',
        (e) => {
          calendarFocusEventId = e.payload.eventId
          currentView = 'calendar'
        },
      )
      // #132: backend fires this whenever a custom theme is
      // imported / removed (in this window or another).  Re-pull
      // settings so the picker + the runtime <link> registry
      // both refresh without a full reload.
      unlistenCustomThemes = await listen('custom-themes-changed', () =>
        loadAppPrefs(),
      )
      unlistenCompose = await listen('open-compose', () => openCompose({}))
      // Standalone-mail windows (#104) emit these when the user
      // hits Reply / Reply All / Forward over there: we run the
      // existing compose flow here in the main window so the user
      // ends up with one Compose surface, with all autocomplete
      // and signature state already wired up.
      unlistenComposeFromMail = await listen<{
        kind: 'reply' | 'reply-all' | 'forward'
        mail: ReplyableMail
      }>('compose-from-mail', (e) => {
        const { kind, mail } = e.payload
        if (kind === 'reply') onReply(mail)
        else if (kind === 'reply-all') onReplyAll(mail)
        else if (kind === 'forward') onForward(mail)
      })
      unlistenEditDraftFromMail = await listen<{ mail: DraftMail }>(
        'edit-draft-from-mail',
        (e) => onEditDraft(e.payload.mail),
      )
      unlistenMailtoFromMail = await listen<{
        init: { to?: string; cc?: string; bcc?: string; subject?: string; body?: string }
      }>('mailto-from-mail', (e) => openCompose(e.payload.init))

      // #254 — when Nimbus is launched as the OS handler for an
      // .ics or .eml file (Windows registry / macOS UTI / Linux
      // .desktop), the backend stashes the path in a one-shot
      // slot during process startup.  Pull it now, after the
      // event listeners are wired so an iCal "Show event" path
      // can still race past us if it ever overlaps.  Best-effort:
      // any failure is logged and dropped so a malformed handoff
      // doesn't keep the user staring at an empty app shell.
      void processPendingLaunchFile()
    })()
    return () => {
      unlistenNewMail?.()
      unlistenEventReminder?.()
      unlistenReminderShowEvent?.()
      unlistenCustomThemes?.()
      unlistenCompose?.()
      unlistenComposeFromMail?.()
      unlistenEditDraftFromMail?.()
      unlistenMailtoFromMail?.()
      unlistenMailFlagsUpdated?.()
      unlistenOutboxUpdated?.()
      if (pendingSummaryTimer) clearTimeout(pendingSummaryTimer)
    }
  })

  async function checkAccounts() {
    try {
      const list = await invoke<Account[]>('get_accounts')
      accounts = list
      if (list.length > 0) {
        // Keep the current selection if it still exists (e.g. after
        // adding another account); otherwise fall back to the
        // visually-first account.  `list` is in insertion order; the
        // IconRail and sidebar render by `sort_order`, so we sort
        // here to match — otherwise the auto-picked account isn't
        // the one the user sees at the top of the rail (#161).
        if (!activeAccountId || !list.some((a) => a.id === activeAccountId)) {
          const sorted = [...list].sort((a, b) => {
            const ao = a.sort_order ?? 0
            const bo = b.sort_order ?? 0
            if (ao !== bo) return ao - bo
            return a.id.localeCompare(b.id)
          })
          activeAccountId = sorted[0].id
        }
        currentView = 'inbox'
      } else {
        activeAccountId = null
        currentView = 'setup'
      }
    } catch {
      // If we can't load accounts (e.g. first launch, file doesn't exist),
      // show the setup wizard
      accounts = []
      activeAccountId = null
      currentView = 'setup'
    }
  }

  // ── Navigation handlers ─────────────────────────────────────
  function goToInbox() {
    currentView = 'inbox'
    // The user may have added / removed accounts in settings; re-read
    // the list so the IconRail avatars and the active selection
    // reflect the current state.
    void checkAccounts()
  }

  /**
   * Switch the app to a different mail account. Called by the Sidebar
   * account picker. IMAP UIDs are per-account so keeping `selectedUid`
   * would point at a message that doesn't exist in the new account;
   * resetting folder → INBOX keeps the landing experience predictable.
   * Also clears search state because the query was scoped to the old
   * account.
   *
   * The sentinel `"__all__"` toggles `unifiedMode` instead of changing
   * the active account — `activeAccountId` stays pointed at whatever
   * the user had before so the sidebar folder tree and integrations
   * still have a sensible default. Pinging back into a real account
   * id automatically turns unified mode off.
   */
  function selectAccount(id: string) {
    // Picking an account from the IconRail always means "show me
    // mail for this account" — even from calendar / contacts /
    // settings, where the rail is still visible.  Flip the view
    // before any of the early-return paths so a click from
    // another view always lands you in the inbox (#161).
    const wasOnMail = currentView === 'inbox'
    if (currentView !== 'inbox') currentView = 'inbox'

    // Refresh on every IconRail click (#196): the user clicking
    // an account avatar is an explicit "check this now" gesture,
    // even when that avatar is already selected (early-return
    // paths below skip the state reset, but the poll still
    // fires). New envelopes flow in via the existing `new-mail`
    // / `unread-count-updated` events the MailList already
    // subscribes to — no extra wiring needed.
    //
    // Wrap the invoke in a refresh flag (#196 follow-up) so the
    // active avatar's spinner ring shows for the duration of
    // the poll, even when re-clicking the already-active avatar
    // (which otherwise wouldn't trigger MailList's own
    // refreshing flag).
    accountClickRefreshing = true
    void invoke('check_mail_now')
      .catch((e) => {
        console.warn('check_mail_now from IconRail click failed', e)
      })
      .finally(() => {
        accountClickRefreshing = false
      })

    if (id === '__all__') {
      if (unifiedMode && wasOnMail) return
      unifiedMode = true
      selectedFolder = 'INBOX'
      selectedUid = null
      selectedMessageAccountId = null
      searchQuery = ''
      searchScope = {}
      searchFilters = {}
      refreshToken++
      return
    }
    if (!unifiedMode && id === activeAccountId && wasOnMail) return
    unifiedMode = false
    activeAccountId = id
    selectedFolder = 'INBOX'
    selectedUid = null
    selectedMessageAccountId = null
    searchQuery = ''
    searchScope = {}
    searchFilters = {}
    refreshToken++
  }

  function goToSetup() {
    currentView = 'setup'
  }

  /** IconRail nav click. Maps the rail's view enum directly to the
   *  router's `currentView` — the old `onSelectIntegration` took
   *  string labels like "Contacts" / "Nextcloud Talk" because the
   *  Sidebar rendered those display names verbatim; the rail uses
   *  a typed `RailView` instead so this handler is just a
   *  structural pass-through with no case map. */
  function onSelectView(view: RailView) {
    currentView = view
  }

  /** Fire a `check_mail_now` whenever the user transitions into the
   *  mail view. The background sync loop already runs on its own
   *  cadence (`background_sync_interval_secs`, default 60s), but a
   *  fresh poll on view-switch matches what users expect — the
   *  mailbox you just opened should reflect the server, not whatever
   *  state the background loop last landed. The first run fires on
   *  initial load into `'inbox'`, which is redundant with the bg
   *  loop's startup poll but cheap and predictable. */
  $effect(() => {
    if (currentView === 'inbox') {
      void invoke('check_mail_now').catch((e) =>
        console.warn('auto check_mail_now on view switch failed:', e),
      )
    }
  })

  async function onSetupComplete() {
    // After adding an account, refresh the account list so we pick
    // up the new account's ID, then switch to the inbox.
    await checkAccounts()
    currentView = 'inbox'
  }

  function selectMessage(uid: number, accountId?: string) {
    selectedUid = uid
    // Unified mode: each row carries its owning account id so MailView
    // can fetch from the right account. Outside unified mode, the
    // active account is implicit.
    selectedMessageAccountId = accountId ?? null
  }

  // Changing the folder resets the open message — the UID that was
  // selected doesn't exist in the new folder, so showing it would be
  // stale at best.
  function selectFolder(name: string) {
    selectedFolder = name
    selectedUid = null
    // Clear the Outbox preview when switching away — the
    // right-pane routing is folder-conditional, but the
    // selectedOutboxRow value would otherwise linger and
    // re-show the preview the next time the user lands back
    // on the Outbox folder.
    if (name !== OUTBOX_FOLDER) {
      selectedOutboxRow = null
    }
  }

  // MailView fires this after it successfully marks a message \Seen
  // on the server.  Used to bump `refreshToken` to force a full
  // MailList reload, but that races against the user's next click —
  // the `fetch_envelopes` IMAP call is in flight when the next
  // optimistic action runs, then lands and overwrites the local
  // list (#174 follow-up).  Flip the matching envelope's flag in
  // the bound list directly instead; the cache row was already
  // updated by the backend, and the per-account unread badge is
  // driven by its own `unread-count-by-account-updated` event.
  function onMessageRead(uid: number) {
    const idx = mailListEnvelopes.findIndex((e) => e.uid === uid)
    if (idx >= 0 && !mailListEnvelopes[idx].is_read) {
      mailListEnvelopes[idx].is_read = true
    }
  }

  /** The currently shown message has been archived or deleted on the
   *  server.  Auto-advances the reading pane to the row directly
   *  below the removed one (or the row above when the removed row
   *  was last) so triage flows don't bounce back to the empty
   *  "pick a message" placeholder after every delete / archive
   *  click.  Falls back to clearing the pane when the list is now
   *  empty, when we can't find the removed UID in the current
   *  rendered list, or when the user has explicitly opted out via
   *  `appPrefs.auto_advance_after_remove`. */
  function onMessageRemoved(removedUid: number) {
    // Auto-advance only fires when the removed message is the one
    // currently open in the reading pane.  For drag-and-drop moves
    // (#89) the user typically drags a non-selected row to a folder
    // — yanking the pane to that row's neighbour would be
    // disorienting, so we leave the current selection alone.
    const wasSelected = selectedUid === removedUid

    if (wasSelected) {
      let nextUid: number | null = null
      let nextAccountId: string | null = null

      if (appPrefs?.auto_advance_after_remove ?? true) {
        const idx = mailListEnvelopes.findIndex((e) => e.uid === removedUid)
        if (idx >= 0) {
          // Visually the list is sorted newest-first, so the row
          // "below" the current one is `idx + 1` (older message).
          // When the removed row was at the bottom, we step up to
          // `idx - 1` instead, matching what every mainstream
          // client does after deleting the oldest visible mail.
          const next = mailListEnvelopes[idx + 1] ?? mailListEnvelopes[idx - 1]
          if (next) {
            nextUid = next.uid
            nextAccountId = next.account_id || null
          }
        }
      }

      selectedUid = nextUid
      selectedMessageAccountId = nextAccountId
    }

    // Drop the matching envelope from the bound list (#174
    // follow-up).  MailList's own optimistic delete/move already
    // removed the row from its internal `envelopes`, in which
    // case this is a no-op.  The path that *needs* this is
    // Sidebar's drag-and-drop drop handler — it fires
    // `onmessagemoved` per UID but doesn't touch MailList's
    // state directly, so without this splice the moved row stays
    // visible until a folder switch, and clicking it lands on a
    // UID the cache has already dropped → "no message with UID".
    const idx = mailListEnvelopes.findIndex((e) => e.uid === removedUid)
    if (idx >= 0) {
      mailListEnvelopes = [
        ...mailListEnvelopes.slice(0, idx),
        ...mailListEnvelopes.slice(idx + 1),
      ]
    }

    // Deliberately *not* bumping `refreshToken` here.  After the
    // optimistic flow Phase 1 already dropped the row from
    // MailList's local list and Phase 2 tombstoned the cache row,
    // so any reload would just race a `fetch_envelopes` IMAP
    // call against the next click — making sequential deletes
    // feel laggy because the second click hits a list mid-
    // network-refresh.  Background sync's `new-mail` event drives
    // the genuine refresh path; this one's purely local.
  }

  // Open the Compose modal. Called with no arg for a blank new message,
  // or with a prefill for reply/reply-all/forward.
  function openCompose(initial: ComposeInitial = {}) {
    // A fresh open shouldn't carry over the error banner from a
    // previous failed background send (#156).
    composeSendError = ''
    composeInitial = initial
    // Snapshot whether this Compose started life as an
    // edit-from-outbox — onsentenqueued reads it later (#276
    // follow-up).  Re-set on every open so a fresh non-edit
    // compose can't inherit the arming from a previous edit
    // that the user cancelled.
    composeOpenedAsEditOfOutbox = initial.outboxSource != null
  }

  /** Re-open a queued Outbox message in Compose for editing
   *  (#276).  Peeks at the row without removing it: cancelling
   *  Compose leaves the original queued copy alone, sending
   *  routes through `send_email` with `outboxSource: { id }` so
   *  the backend drops the source row atomically with
   *  enqueueing the edit.
   *
   *  `skipSignatureInsert` is true so Compose doesn't stack a
   *  second signature on top of the one already embedded in
   *  the queued body. */
  function onEditOutbox(row: OutboxRowDto) {
    let outgoing: {
      from: string
      to: string[]
      cc: string[]
      bcc: string[]
      reply_to: string | null
      subject: string
      body_text: string | null
      body_html: string | null
      attachments: unknown[]
    }
    try {
      outgoing = JSON.parse(row.outgoingJson)
    } catch (e) {
      alert(`Stored Outbox payload was unreadable: ${e}`)
      return
    }
    let repliedTo: ComposeInitial['repliedTo']
    if (row.repliedToJson) {
      try {
        const parsed = JSON.parse(row.repliedToJson)
        if (parsed && typeof parsed === 'object') {
          repliedTo = {
            accountId: row.accountId,
            folder: parsed.folder,
            uid: parsed.uid,
            kind: parsed.kind,
          }
        }
      } catch (e) {
        console.warn('outbox repliedToJson parse failed', e)
      }
    }
    openCompose({
      to: outgoing.to.join(', '),
      cc: outgoing.cc.length > 0 ? outgoing.cc.join(', ') : undefined,
      bcc: outgoing.bcc.length > 0 ? outgoing.bcc.join(', ') : undefined,
      subject: outgoing.subject,
      body: outgoing.body_html || outgoing.body_text || '',
      attachments: outgoing.attachments as never,
      repliedTo,
      outboxSource: { id: row.id },
      skipSignatureInsert: true,
    })
  }

  function closeCompose() {
    composeInitial = null
    // Force the mail list + sidebar to re-query the server. Compose's
    // save-draft / send paths modify the Drafts and Sent folders
    // (APPEND + expunge) without touching the envelope cache, so the
    // UI would otherwise stay on the pre-compose view until the user
    // clicked another folder.
    refreshToken++
  }

  // ── Background-send failure recovery (#156) ─────────────────
  // Compose now closes the modal as soon as the user clicks Send;
  // the IMAP submission runs in the background.  When that
  // submission fails after the modal is gone we surface the
  // error here AND re-open Compose pre-filled with the user's
  // draft so they can retry without retyping.
  let composeSendError = $state<string>('')
  function onComposeSendFailed(payload: SendFailurePayload) {
    composeSendError = payload.errorMessage
    // Re-open Compose with the original draft.  Setting
    // `composeInitial` triggers the mount in the same shell-
    // level branch the original Compose lived in.
    composeInitial = payload.draft
    // Try to also fire an OS-level notification so the user
    // notices the failure even if their attention has drifted
    // off the Nimbus window.  Best-effort — silently ignore on
    // platforms / permissions where it can't post.
    if (notificationsGranted) {
      try {
        sendNotification({
          title: 'Nimbus Mail — send failed',
          body: payload.errorMessage,
          icon: notificationIconPath || undefined,
        })
      } catch (e) {
        console.warn('send-failed notification failed', e)
      }
    }
  }

  /** Fires when Compose's send invoke succeeds with the new
   *  outbox row's id (#276 follow-up).  Distinct from `onclose`
   *  because the modal closes immediately on Send (#156's
   *  instant-close UX) — relying on `onclose` alone makes
   *  cancel and send indistinguishable.
   *
   *  When the just-sent Compose started life as an
   *  edit-from-outbox, look up the new row in the queue and
   *  surface it as the selected Outbox preview.  Three
   *  outcomes:
   *
   *    * Healthy network: the drain task may have already
   *      removed the row by the time this lookup runs — the
   *      list comes back without the id, and we leave the
   *      selection cleared (the empty-Outbox auto-route in
   *      `outbox-updated` then takes over).
   *    * Failed send: the row is still in the queue with a
   *      `last_error`; we select it so the user can see what
   *      went wrong without manually clicking the row.
   *    * Mid-flight: row exists with no error yet — same
   *      select behaviour, the row's status updates in place
   *      via the `outbox-updated` listener as the drain
   *      finishes. */
  async function onComposeSentEnqueued(newRowId: number) {
    if (!composeOpenedAsEditOfOutbox) return
    composeOpenedAsEditOfOutbox = false
    // Stay on the Outbox folder (the user was here when they
    // clicked Edit; switching away would be confusing).
    selectedFolder = OUTBOX_FOLDER
    selectedUid = null
    try {
      const rows = await invoke<OutboxRowDto[]>('list_outbox', {
        accountId: activeAccountId ?? '',
      })
      selectedOutboxRow = rows.find((r) => r.id === newRowId) ?? null
    } catch (e) {
      console.warn('list_outbox after edit-send failed', e)
    }
  }

  /** Build a quoted reply body as HTML.
   *
   *  Output shape (the Compose editor's `initialBodyHtml` accepts
   *  literal HTML — it detects tags and passes the string straight
   *  through instead of escaping):
   *
   *    <p></p>
   *    <p></p>
   *    <p>On <date>, <from> wrote:</p>
   *    <blockquote>...original body, escaped or passed-through...</blockquote>
   *
   *  The two leading empty paragraphs give the user a visible cursor
   *  above the quote to start typing into. The `<blockquote>` is
   *  Tiptap-native, so the styling we already have (left bar, indent,
   *  muted colour) applies; when the message is sent, the HTML flows
   *  straight through to the wire so the recipient's client renders
   *  it the same way every other client does.
   */
  /** Build a styled quoted-history HTML block for the previous
   *  thread.  Returned as a standalone block, NOT spliced into
   *  the editor body — Tiptap's schema unwraps generic <div>
   *  wrappers and strips inline styles, so we keep this out of
   *  the editor and let Compose render it as its own read-only
   *  preview block + splice it in at send time. */
  function quoteBody(from: string, date: string, body: string | null): string {
    const bodyHtml = htmlOrEscape(body ?? '')
    const when = new Date(date).toLocaleString()
    return quotedHistoryHtml({
      fromHeader: from,
      whenText: when,
      bodyHtml,
    })
  }

  /** If the input already looks like HTML, pass it through. Otherwise
   *  escape special chars and convert newlines to `<br>` so the plain
   *  text renders with its original line breaks inside the blockquote. */
  function htmlOrEscape(text: string): string {
    if (/<[a-z][\s\S]*>/i.test(text)) return text
    return text
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/\n/g, '<br>')
  }

  function replySubject(s: string): string {
    return /^re:/i.test(s) ? s : `Re: ${s}`
  }

  function forwardSubject(s: string): string {
    return /^fwd?:/i.test(s) ? s : `Fwd: ${s}`
  }

  type OpenMail = {
    from: string
    to: string[]
    cc: string[]
    subject: string
    body_text: string | null
    date: string
    /** RFC 5322 threading anchors (#277).  Optional because
     *  older cached payloads predate the parser; absent values
     *  just mean the reply we send won't carry an In-Reply-To
     *  pointing here. */
    message_id?: string | null
    in_reply_to?: string | null
    references_ids?: string[]
  }

  /** Reply / reply-all / "respond with meeting" need to know
   *  (account_id, folder, uid) so the answered-tracking flow
   *  (#255) can flip `\Answered` on the original message after
   *  a successful send.  MailView passes them through via
   *  `{...email, uid}`; the standalone-window emit threads the
   *  same shape across windows. */
  type ReplyableMail = OpenMail & {
    account_id: string
    folder: string
    uid: number
  }

  function onReply(mail: ReplyableMail) {
    openCompose({
      to: mail.from,
      subject: replySubject(mail.subject),
      body: quoteBody(mail.from, mail.date, mail.body_text),
      repliedTo: {
        accountId: mail.account_id,
        folder: mail.folder,
        uid: mail.uid,
        kind: 'reply',
        // RFC 5322 threading anchors (#277).  Carrying these on
        // the reply context lets Compose stamp `In-Reply-To` /
        // `References` on the outgoing message so other clients
        // group it with the parent.
        parentMessageId: mail.message_id ?? null,
        parentReferences: mail.references_ids ?? [],
      },
    })
  }

  function onReplyAll(mail: ReplyableMail) {
    const others = [...mail.to, ...mail.cc].filter(
      (a) => a && a.toLowerCase() !== activeAccountEmail.toLowerCase(),
    )
    openCompose({
      to: mail.from,
      cc: others.join(', '),
      subject: replySubject(mail.subject),
      body: quoteBody(mail.from, mail.date, mail.body_text),
      repliedTo: {
        accountId: mail.account_id,
        folder: mail.folder,
        uid: mail.uid,
        kind: 'reply-all',
        parentMessageId: mail.message_id ?? null,
        parentReferences: mail.references_ids ?? [],
      },
    })
  }

  /** Does the given folder name look like the account's Drafts folder?
   *  Mirrors the Rust-side `pick_drafts_folder` name-hint list (the
   *  authoritative `\Drafts` special-use attribute lives on the server
   *  and we don't propagate it to the frontend yet, so this is the
   *  pragmatic "good enough" heuristic). */
  const DRAFTS_NAME_HINTS = ['drafts', 'draft', 'entwürfe', 'entwurf', 'brouillons', 'brouillon']
  function isDraftsFolderName(name: string): boolean {
    const lower = name.toLowerCase()
    return DRAFTS_NAME_HINTS.some((h) => lower.includes(h))
  }
  const isDraftsFolder = $derived(isDraftsFolderName(selectedFolder))

  /** Same heuristic for the Sent folder — used to suppress the
   *  RSVP card on outbound invites the user themselves sent
   *  (you don't reply to your own meeting requests).  Same
   *  caveat as the Drafts hint: name-based until the backend
   *  surfaces `\Sent` special-use through the API. */
  const SENT_NAME_HINTS = ['sent', 'sent items', 'gesendet', 'envoyés', 'envoyes', 'inviati', 'enviados']
  function isSentFolderName(name: string): boolean {
    const lower = name.toLowerCase()
    return SENT_NAME_HINTS.some((h) => lower.includes(h))
  }
  const isSentFolder = $derived(isSentFolderName(selectedFolder))

  /** Open a draft from the Drafts folder back in Compose for editing.
   *  Mirrors the reply/forward entry points but additionally:
   *    - downloads every attachment's bytes so the user can re-send
   *      or re-save without the attachments silently dropping;
   *    - records the source UID/folder in `draftSource` so Compose
   *      can expunge the server-side copy once the edit is sent or
   *      re-saved (otherwise the Drafts mailbox accumulates one
   *      copy per edit).
   *  The reply-style guard fields (`in_reply_to`) stay unset: this is
   *  a continuation of the user's own work, not a response to someone
   *  else, so the signature effect correctly skips re-inserting. */
  type DraftMail = OpenMail & {
    account_id: string
    folder: string
    bcc?: string[]
    body_html: string | null
    attachments: { filename: string; content_type: string; part_id: number }[]
  }
  async function onEditDraft(mail: DraftMail) {
    if (selectedUid == null) return
    const uid = selectedUid
    // Pull every attachment's bytes. Parallel — even mid-size drafts
    // rarely have more than a couple of attachments, and the IMAP
    // backend already reuses one connection per `fetch_message`
    // command internally.
    const attachments = await Promise.all(
      mail.attachments.map(async (att) => ({
        filename: att.filename,
        content_type: att.content_type,
        data: await invoke<number[]>('download_email_attachment', {
          accountId: mail.account_id,
          folder: mail.folder,
          uid,
          partId: att.part_id,
        }),
        // Fresh content_id — the `/` editor shortcut references
        // attachments by this id, so each one needs a value even
        // when we're rehydrating a draft. Any `cid:` refs already
        // baked into the old draft body are intentionally broken
        // by this: fixing them up would mean parsing the stored
        // HTML and rewriting refs, which is scope-heavier and can
        // wait. For a freshly-edited draft you just re-pick the
        // attachment via `/` to relink.
        content_id: crypto.randomUUID().replaceAll('-', ''),
      })),
    )
    openCompose({
      to: mail.to.join(', '),
      cc: mail.cc.join(', '),
      bcc: (mail.bcc ?? []).join(', '),
      subject: mail.subject,
      // Prefer the HTML body — the editor is a rich-text editor and
      // will pass the HTML through unchanged (`textToHtml` detects
      // tags). Fall back to plain text for the rare HTML-less draft.
      body: mail.body_html ?? mail.body_text ?? '',
      attachments,
      draftSource: { accountId: mail.account_id, folder: mail.folder, uid },
    })
  }

  function onForward(mail: OpenMail) {
    // Forwards use the same blockquote treatment as replies so the
    // original message sits inside a visually distinct container.
    // Unlike reply, we prefix with a small header block that states
    // the original From/Date/Subject so the recipient can see the
    // chain even if they collapse the quote.
    const esc = (s: string) =>
      s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    const when = new Date(mail.date).toLocaleString()
    const header =
      `<p><strong>---------- Forwarded message ----------</strong></p>` +
      `<p>From: ${esc(mail.from)}<br>` +
      `Date: ${esc(when)}<br>` +
      `Subject: ${esc(mail.subject)}</p>`
    const body = htmlOrEscape(mail.body_text ?? '')
    openCompose({
      subject: forwardSubject(mail.subject),
      body:
        `<p></p><p></p>` +
        `<blockquote>${header}${body}</blockquote>`,
    })
  }

  // ── "Respond with meeting" flow ────────────────────────────
  // Triggered from MailView's meeting button. Opens the full
  // EventEditor pre-filled with the email subject as the title, the
  // thread's From/To as required attendees, Cc as optional, and
  // (via `createTalkRoom: true`) auto-creates a Nextcloud Talk room
  // whose join URL lands in the event's location.  One gesture
  // turns an email into a calendar invite plus a meeting link.
  interface CalendarSummary {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    hidden?: boolean
    muted?: boolean
    /** CalDAV-derived read-only flag (#236) — passed through
     *  to EventEditor so the picker / Delete button know not
     *  to offer writes against read-only shared calendars. */
    read_only?: boolean
  }
  let meetingDraft = $state<{
    calendars: CalendarSummary[]
    draft: {
      calendarId: string
      start: Date
      end: Date
      summary: string
      description?: string
      location?: string
      url?: string
      requiredAttendees: string[]
      optionalAttendees: string[]
      chairAttendees?: string[]
      createTalkRoom: boolean
    }
    /** Original thread the user clicked "Respond with meeting"
     *  on — held here so `onMeetingEditorSaved` can reopen
     *  Compose pre-filled as a reply once the event lands
     *  (#195).  Absent when the editor was opened from a
     *  source other than an email thread (e.g. an .ics file
     *  the OS handed us via "Open with…", #254).  Carries the
     *  `ReplyableMail` shape so the post-save Compose knows the
     *  (account_id, folder, uid) needed to flip `\Answered`
     *  (#255). */
    replyTo?: ReplyableMail
  } | null>(null)

  /** Strip an `"Name" <addr>` wrapper down to the bare email. */
  function bareEmail(s: string): string | null {
    const t = s.trim()
    if (!t) return null
    const m = t.match(/^\s*(?:"[^"]*"|[^<]*?)\s*<([^>]+)>\s*$/)
    return m ? m[1].trim() : t
  }

  /** Round a Date up to the next half-hour boundary.  Mirrors what
      a user would type when scheduling a fresh meeting "now-ish":
      11:07 → 11:30, 11:30 → 12:00. */
  /** Prefix the email subject with "Re: " to mark the event as a
      response to the thread.  Skips the prefix when the subject
      already starts with Re:/Aw:/Sv: (case-insensitive) so we don't
      stack "Re: Re: Re:" on a long reply chain. */
  function meetingSubject(subject: string): string {
    const s = subject.trim()
    if (!s) return 'Re: Meeting'
    if (/^(re|aw|sv)\s*:/i.test(s)) return s
    return `Re: ${s}`
  }

  function nextHalfHour(d: Date): Date {
    const out = new Date(d)
    out.setSeconds(0, 0)
    const m = out.getMinutes()
    out.setMinutes(m < 30 ? 30 : 60)
    return out
  }

  async function onRespondWithMeeting(mail: ReplyableMail) {
    // Defensive: if a previous flow left state behind (e.g. a
    // Compose modal whose overlay would z-index over the new
    // editor), clear it so the EventEditor lands on top of an
    // empty surface.
    meetingDraft = null
    composeInitial = null

    // Top-level try/catch so any unhandled rejection surfaces as
    // a visible alert instead of leaving the user staring at a
    // button that did nothing.  The inner blocks still run their
    // own try/catch where they can give a more specific message.
    try {
      let ncId = ''
      try {
        const list = await invoke<{ id: string }[]>('get_nextcloud_accounts')
        if (list.length === 0) {
          alert('Connect a Nextcloud account first (Settings → Nextcloud).')
          return
        }
        ncId = list[0].id
      } catch (e) {
        alert(`Failed to load Nextcloud accounts: ${e}`)
        return
      }

      let calendars: CalendarSummary[] = []
      try {
        calendars = await invoke<CalendarSummary[]>('get_cached_calendars', { ncId })
      } catch (e) {
        alert(`Failed to load calendars: ${e}`)
        return
      }
      const visible = calendars.filter((c) => !c.hidden)
      if (visible.length === 0) {
        alert('No writable calendars found on your Nextcloud account.')
        return
      }
      let initialCalendarId = visible[0].id
      try {
        const s = await invoke<{ default_calendar_id: string | null }>('get_app_settings')
        if (s.default_calendar_id && visible.some((c) => c.id === s.default_calendar_id)) {
          initialCalendarId = s.default_calendar_id!
        }
      } catch {}

      // Split the thread's participants — From + To go required,
      // Cc goes optional.  Skip the active account (the user is the
      // organizer; the editor adds them as CHAIR).  De-dupe across
      // buckets so an address that appears in both To and Cc only
      // shows up once in the higher-priority bucket.
      const self = activeAccountEmail.toLowerCase()
      const seen = new Set<string>()
      const required: string[] = []
      for (const piece of [mail.from, ...mail.to]) {
        const addr = bareEmail(piece)
        if (!addr) continue
        const key = addr.toLowerCase()
        if (key === self || seen.has(key)) continue
        seen.add(key)
        required.push(piece)
      }
      const optional: string[] = []
      for (const piece of mail.cc) {
        const addr = bareEmail(piece)
        if (!addr) continue
        const key = addr.toLowerCase()
        if (key === self || seen.has(key)) continue
        seen.add(key)
        optional.push(piece)
      }

      const start = nextHalfHour(new Date())
      const end = new Date(start.getTime() + 30 * 60 * 1000)

      meetingDraft = {
        calendars: visible,
        draft: {
          calendarId: initialCalendarId,
          start,
          end,
          summary: meetingSubject(mail.subject),
          requiredAttendees: required,
          optionalAttendees: optional,
          createTalkRoom: true,
        },
        replyTo: mail,
      }
    } catch (e) {
      alert(`Failed to open meeting editor: ${e}`)
    }
  }

  function onMeetingEditorClose() {
    meetingDraft = null
  }
  function onMeetingEditorSaved(saved?: SavedEvent) {
    // Keep a local snapshot before clearing — the editor is
    // dismissed first so its surface unmounts before Compose
    // mounts on top, avoiding a brief two-modal frame.
    const ctx = meetingDraft
    meetingDraft = null
    if (!saved || !ctx) return

    // The Talk URL gets written into LOCATION when "Make it a
    // Talk conversation" is checked (mirrors what NC's Calendar
    // app does — keeps an iCalendar-canonical place for the
    // join link).  We split the field heuristically: if the
    // location starts with http(s)://, treat it as the Talk URL
    // and leave the physical-location row empty; otherwise it's
    // a real address / room name and there's no Talk URL.
    const loc = (saved.location ?? '').trim()
    const isUrl = /^https?:\/\//i.test(loc)
    const meetingInvite: MeetingInvite = {
      summary: saved.summary,
      start: saved.start,
      end: saved.end,
      location: isUrl ? null : loc || null,
      description: saved.description ?? null,
      talkUrl: isUrl ? loc : null,
    }

    // No source thread (e.g. the editor was opened from an .ics
    // file we got handed via the OS, #254) — just save the event
    // and bow out.  No reply Compose makes sense without an
    // email to reply to.
    if (!ctx.replyTo) return

    // Open Compose as a reply to the original thread, with the
    // styled meeting card pre-filled into the body.  Existing
    // reply ergonomics (To from From, Re: subject prefix, quoted
    // body) match what `onReply` already produces.
    const original = ctx.replyTo
    const others = [...original.to, ...original.cc].filter(
      (a) => a && a.toLowerCase() !== activeAccountEmail.toLowerCase(),
    )
    openCompose({
      to: original.from,
      cc: others.length > 0 ? others.join(', ') : undefined,
      subject: replySubject(original.subject),
      // Body holds the styled quoted-history block; the meeting
      // card lands above it via `initialBodyHtml` in Compose,
      // which prepends `meetingInvite`-rendered HTML.
      body: quoteBody(original.from, original.date, original.body_text),
      meetingInvite,
      // #255 — flag the original as `\Answered` once the meeting
      // reply lands.  The icon distinguishes "respond with
      // meeting" from a plain reply or reply-all.
      repliedTo: {
        accountId: original.account_id,
        folder: original.folder,
        uid: original.uid,
        kind: 'meeting',
      },
    })
  }

  // ── OS file-association handoff (#254) ─────────────────────────
  // When the user launches Nimbus by double-clicking an .ics /
  // .eml file (or via "Open with… → Nimbus"), the OS hands the
  // path to the process as `argv[1]`.  The Rust side stashes it
  // in a one-shot slot during process startup; we drain it here
  // and dispatch by extension:
  //
  //   .eml → spawn a view-only popout window (read-only by
  //          design — no account context, no folder, so reply /
  //          archive don't make sense)
  //   .ics → load the user's calendars, parse the file into
  //          our CalendarEvent shape, prefill EventEditor in
  //          create-mode so the user can adjust the title /
  //          time / attendees and save into a calendar of their
  //          choice (per the user's directive — NOT read-only)

  /** Lift a `CalendarEvent.attendees` list into the
   *  required / optional / chair buckets EventEditor's draft
   *  prop expects.  Each attendee is rendered as `"Name" <addr>`
   *  when CN is present, or just the bare address otherwise —
   *  matches `parseAddress` everywhere else. */
  function bucketAttendeesByRole(
    attendees: { email: string; common_name?: string | null; role?: string | null }[],
    self: string,
  ): {
    required: string[]
    optional: string[]
    chair: string[]
  } {
    const required: string[] = []
    const optional: string[] = []
    const chair: string[] = []
    const selfAddr = self.toLowerCase()
    for (const a of attendees) {
      const email = (a.email || '').replace(/^mailto:/i, '').trim()
      if (!email) continue
      if (email.toLowerCase() === selfAddr) continue
      const piece = a.common_name ? `"${a.common_name}" <${email}>` : email
      const role = (a.role || 'REQ-PARTICIPANT').toUpperCase()
      if (role === 'OPT-PARTICIPANT') optional.push(piece)
      else if (role === 'CHAIR') chair.push(piece)
      else required.push(piece)
    }
    return { required, optional, chair }
  }

  /** Drain the one-shot launch-file slot in the Rust backend
   *  and dispatch the path by extension.  Called once during
   *  the main app's startup `$effect`. */
  async function processPendingLaunchFile() {
    let path: string | null = null
    try {
      path = await invoke<string | null>('take_pending_file_to_open')
    } catch (e) {
      console.warn('take_pending_file_to_open failed', e)
      return
    }
    if (!path) return

    const lower = path.toLowerCase()
    if (lower.endsWith('.eml')) {
      try {
        await openMailFileInStandaloneWindow(path)
      } catch (e) {
        console.warn('openMailFileInStandaloneWindow failed', e)
      }
      return
    }
    if (lower.endsWith('.ics')) {
      await openIcsFileInEditor(path)
      return
    }
    console.warn('pending file has unsupported extension:', path)
  }

  /** Subset of the Rust `CalendarEvent` shape we read off of
   *  `parse_ics_file`.  EventEditor and CalendarView each
   *  define their own internal interface for the same data;
   *  rather than reach in and import a non-exported type, we
   *  stamp out the fields we actually need here. */
  interface ImportedIcsEvent {
    summary: string
    description?: string | null
    start: string
    end: string
    location?: string | null
    url?: string | null
    attendees?: { email: string; common_name?: string | null; role?: string | null }[]
  }

  /** Parse an .ics on disk and open EventEditor in create-mode
   *  pre-filled with the parsed event's data.  Multiple VEVENTs
   *  in one file are uncommon for hand-shared invites — we use
   *  the first one and let the user save it; the others are
   *  ignored.  The user picks the calendar in the editor. */
  async function openIcsFileInEditor(path: string) {
    let events: ImportedIcsEvent[] = []
    try {
      events = await invoke<ImportedIcsEvent[]>('parse_ics_file', { path })
    } catch (e) {
      alert(`Could not parse the calendar file: ${e}`)
      return
    }
    if (events.length === 0) {
      alert('The calendar file did not contain any events.')
      return
    }
    const ev = events[0]

    // Calendars come from the user's first connected Nextcloud
    // account (same source the "Respond with meeting" flow
    // uses).  Without an NC account there's nowhere to save
    // the event, so we surface that and bail.
    let ncId = ''
    try {
      const list = await invoke<{ id: string }[]>('get_nextcloud_accounts')
      if (list.length === 0) {
        alert(
          'Connect a Nextcloud account first (Settings → Nextcloud) so this event has a calendar to land in.',
        )
        return
      }
      ncId = list[0].id
    } catch (e) {
      alert(`Failed to load Nextcloud accounts: ${e}`)
      return
    }

    let calendars: CalendarSummary[] = []
    try {
      calendars = await invoke<CalendarSummary[]>('get_cached_calendars', { ncId })
    } catch (e) {
      alert(`Failed to load calendars: ${e}`)
      return
    }
    const visible = calendars.filter((c) => !c.hidden)
    if (visible.length === 0) {
      alert('No writable calendars found on your Nextcloud account.')
      return
    }
    let initialCalendarId = visible[0].id
    try {
      const s = await invoke<{ default_calendar_id: string | null }>(
        'get_app_settings',
      )
      if (s.default_calendar_id && visible.some((c) => c.id === s.default_calendar_id)) {
        initialCalendarId = s.default_calendar_id!
      }
    } catch {}

    const buckets = bucketAttendeesByRole(ev.attendees ?? [], activeAccountEmail)

    meetingDraft = {
      calendars: visible,
      draft: {
        calendarId: initialCalendarId,
        start: new Date(ev.start),
        end: new Date(ev.end),
        summary: ev.summary || '',
        description: ev.description ?? undefined,
        location: ev.location ?? undefined,
        url: ev.url ?? undefined,
        requiredAttendees: buckets.required,
        optionalAttendees: buckets.optional,
        chairAttendees: buckets.chair,
        // Don't auto-mint a Talk room for a third-party invite
        // we just imported — the original file may already have
        // a join URL in LOCATION or URL, and creating a fresh
        // room would muddy the invite.
        createTalkRoom: false,
      },
      // No source thread — this is a file the user opened, not
      // a reply path.  `onMeetingEditorSaved` skips its Compose
      // step when this is absent.
    }
  }

  /** "Save as note" handler — issue #67's email→note bridge. Builds
      a markdown body that preserves the headers the user actually
      cares about (From / To / Date) so the note carries enough
      context to be useful when read months later. Body source
      preference: plain text first (already the right shape for
      markdown), falling back to a stripped HTML body so users on
      HTML-only senders still get readable note content. */
  async function onSaveMailAsNote(mail: OpenMail & { body_html?: string | null }) {
    let ncId = ''
    try {
      const list = await invoke<{ id: string }[]>('get_nextcloud_accounts')
      if (list.length === 0) {
        alert('Connect a Nextcloud account first (Settings → Nextcloud).')
        return
      }
      ncId = list[0].id
    } catch (e) {
      alert(`Failed to load Nextcloud accounts: ${e}`)
      return
    }

    const headerLines = [
      `**From:** ${mail.from}`,
      mail.to.length ? `**To:** ${mail.to.join(', ')}` : null,
      mail.cc.length ? `**Cc:** ${mail.cc.join(', ')}` : null,
      `**Date:** ${new Date(mail.date).toLocaleString()}`,
    ].filter(Boolean)

    let body = (mail.body_text ?? '').trim()
    if (!body && mail.body_html) {
      // Strip tags for the markdown note body — collapsing
      // whitespace afterwards keeps the result readable when the
      // sender's HTML had each block on its own line.
      const tmp = document.createElement('div')
      tmp.innerHTML = mail.body_html
      body = (tmp.textContent ?? '').trim()
    }

    const content = `${headerLines.join('  \n')}\n\n---\n\n${body}`
    const title = mail.subject || '(no subject)'

    try {
      await invoke('create_nextcloud_note', {
        ncId,
        title,
        content,
        category: 'Mail',
      })
      // Surface success via the same OS toast path new-mail uses,
      // when permission's been granted; otherwise fall back to a
      // plain alert so the user knows the save took.
      if (notificationsGranted) {
        fireToast('Saved to Notes', title)
      } else {
        alert(`Saved "${title}" to Nextcloud Notes.`)
      }
    } catch (e) {
      alert(`Failed to save note: ${e}`)
    }
  }
</script>

<!-- Lock screen (#164 Phase 1B) — when the cache is in FIDO-only
     mode at boot, the lock screen owns the whole viewport until
     the user authenticates.  Everything else (loading, setup,
     mail / calendar / contacts views) stays unmounted so no IPC
     fires with the cache still locked. -->
<!-- Locale changes require a full app restart (#190): paraglide
     resolves the active locale once at boot from its strategy
     chain (localStorage → preferredLanguage → baseLocale) and
     never re-reads.  In-place reactivity attempts (page
     reload, `{#key}` remount, etc.) all had visible
     side-effects — the language picker in Settings now shows
     a "restart required" prompt instead. -->
{#if dbStatus && dbStatus.locked}
  <LockScreen
    methods={dbStatus.methods}
    attemptsRemaining={dbStatus.attemptsRemaining}
    onattemptschange={(n) => {
      if (dbStatus) dbStatus = { ...dbStatus, attemptsRemaining: n }
    }}
    onunlock={onUnlocked}
  />
{:else if dbStatus === null}
  <!-- Brief flash while we wait for `database_status` to land —
       prevents the loading view from poking the cache before we
       know whether it's locked. -->
  <div class="h-full flex items-center justify-center bg-surface-50 dark:bg-surface-900">
    <p class="text-surface-500">Starting up…</p>
  </div>
{:else if currentView === 'loading'}
  <!-- Loading / Setup both run before the user has an account, so
       the IconRail (which is keyed by accounts) isn't mounted. -->
  <div class="h-full flex items-center justify-center bg-surface-50 dark:bg-surface-900">
    <p class="text-surface-500">Loading...</p>
  </div>
{:else if currentView === 'setup'}
  <!-- The wizard is closeable when the user already has at least
       one account configured (i.e. they reached setup via "Add
       account" from Settings or the IconRail).  On true first
       launch (no accounts) the close affordance is hidden so the
       user has to finish the wizard before they get into the app. -->
  <AccountSetup
    oncomplete={onSetupComplete}
    canCancel={accounts.length > 0}
    oncancel={goToInbox}
  />
{:else}
  <!-- Post-setup shell: IconRail is mounted *once* outside the
       currentView branches, so switching between Mail, Contacts,
       Calendar, Files, Talk, or Settings never remounts the rail
       (keeps the Talk unread poll warm, avatars stable, ring
       transitions smooth). Every view below sits inside the same
       flex row so the rail is always on the far left.

       Compose is also mounted here — it's an overlay modal, so it
       stacks on top of whichever view the user came from without
       the view needing to know about it. -->
  <div class="h-full flex">
    <IconRail
      accounts={accounts}
      accountId={activeAccountId}
      unified={unifiedMode}
      currentView={currentView}
      mailRefreshing={mailRefreshing}
      ncCaps={ncCaps}
      onselectaccount={selectAccount}
      onselectview={onSelectView}
    />

    {#if !activeAccountId}
      <div class="flex-1 flex items-center justify-center bg-surface-50 dark:bg-surface-900">
        <p class="text-surface-500">No account selected.</p>
      </div>
    {:else if currentView === 'settings'}
      <div class="flex-1 min-w-0 overflow-auto">
        <AccountSettings
          onclose={goToInbox}
          onaddaccount={goToSetup}
          onappprefschanged={(p) => (appPrefs = p)}
        />
      </div>
    {:else if currentView === 'contacts'}
      <div class="flex-1 min-w-0">
        <ContactsView onclose={goToInbox} />
      </div>
    {:else if currentView === 'calendar'}
      <div class="flex-1 min-w-0">
        <CalendarView
          onclose={goToInbox}
          focusEventId={calendarFocusEventId}
          oneventfocused={() => (calendarFocusEventId = null)}
        />
      </div>
    {:else if currentView === 'files'}
      <div class="flex-1 min-w-0">
        <FilesView onclose={goToInbox} oncompose={openCompose} />
      </div>
    {:else if currentView === 'talk'}
      <div class="flex-1 min-w-0">
        <TalkView onclose={goToInbox} oncompose={openCompose} />
      </div>
    {:else if currentView === 'notes'}
      <div class="flex-1 min-w-0">
        <NotesView onclose={goToInbox} oncompose={openCompose} />
      </div>
    {:else}
      <!-- Mail view: Sidebar (folders) + mail-list column + MailView.
           Sidebar is now much leaner — just Compose + folder tree —
           since the shell chrome lives on the rail. -->
      <Sidebar
        accounts={accounts}
        accountId={activeAccountId}
        selectedFolder={selectedFolder}
        refreshToken={refreshToken}
        unified={unifiedMode}
        outboxCount={outboxCount}
        onselectfolder={selectFolder}
        oncompose={() => openCompose()}
        onaccountschanged={checkAccounts}
        onmessagemoved={onMessageRemoved}
        onmovesfailed={() => refreshToken++}
      />
      <!-- Mail-list column: SearchBar on top, then either MailList
           or SearchResults depending on whether the user is
           searching. Search isn't wired for unified mode yet —
           searching while unified is enabled scopes back to the
           active account, which is the safer default than silently
           returning nothing. -->
      <div class="flex flex-col w-80 shrink-0 border-r border-surface-200 dark:border-surface-700">
        {#if selectedFolder !== OUTBOX_FOLDER}
          <!-- Hide the search input when the user is sitting on the
               local-only Outbox folder (#276) — there's nothing
               IMAP-side to search there, and the queue's own
               status banner / row text already reads what the
               user needs.  Mounting the search bar would just
               imply a feature that doesn't exist yet. -->
          <SearchBar
            accountId={activeAccountId}
            currentFolder={selectedFolder}
            onsearch={onSearch}
          />
        {/if}
        <div class="flex-1 min-h-0 flex">
          {#if searchActive}
            <SearchResults
              accountId={activeAccountId}
              currentFolder={selectedFolder}
              query={searchQuery}
              scope={searchScope}
              filters={searchFilters}
              selectedUid={selectedUid}
              onselect={onSelectSearchHit}
            />
          {:else if selectedFolder === OUTBOX_FOLDER}
            <!-- #276: Outbox is a local-only folder so we mount a
                 dedicated component instead of feeding synthetic
                 envelopes through MailList.  Same column width;
                 the row template, status banner, and per-row
                 retry/edit/delete actions all live in
                 OutboxList.  Selecting any other folder swaps
                 back to MailList automatically. -->
            <OutboxList
              accountId={activeAccountId ?? ''}
              unified={unifiedMode}
              accounts={accounts}
              refreshToken={outboxRefreshToken}
              selectedId={selectedOutboxRow?.id ?? null}
              onselect={(row) => (selectedOutboxRow = row)}
            />
          {:else}
            <MailList
              accounts={accounts}
              accountId={activeAccountId}
              folder={selectedFolder}
              unified={unifiedMode}
              selectedUid={selectedUid}
              refreshToken={refreshToken}
              onselect={selectMessage}
              bind:envelopes={mailListEnvelopes}
              bind:refreshing={mailListRefreshing}
              onmessagemoved={onMessageRemoved}
            />
          {/if}
        </div>
      </div>
      {#if selectedFolder === OUTBOX_FOLDER && selectedOutboxRow !== null}
        <!-- #276 follow-up: clicking a queued row routes the
             right pane to OutboxView, which renders the
             message's headers + sanitised body so the user can
             see what's about to be sent.  Read-only — the
             retry / edit / delete actions stay on the row in
             OutboxList. -->
        <div class="flex-1 min-w-0">
          <OutboxView row={selectedOutboxRow} onedit={onEditOutbox} />
        </div>
      {:else}
        <MailView
          accountId={selectedMessageAccountId ?? activeAccountId}
          folder={selectedFolder}
          uid={selectedUid}
          forceWhiteBackground={appPrefs?.mail_html_white_background ?? true}
          autoLoadRemoteImages={appPrefs?.auto_load_remote_images ?? false}
          linkCheckEnabled={appPrefs?.link_check_enabled ?? true}
          onread={onMessageRead}
          onreply={onReply}
          onreplyall={onReplyAll}
          onforward={onForward}
          onrespondwithmeeting={onRespondWithMeeting}
          onsavenote={onSaveMailAsNote}
          isDraftsFolder={isDraftsFolder}
          isSentFolder={isSentFolder}
          oneditdraft={onEditDraft}
          onmessageremoved={onMessageRemoved}
          onmailto={(init) => openCompose(init)}
          bind:refreshing={mailViewRefreshing}
        />
      {/if}
    {/if}

    {#if composeInitial !== null}
      <Compose
        accounts={accounts}
        accountId={activeAccountId ?? ''}
        initial={composeInitial}
        initialError={composeSendError}
        onclose={() => {
          composeSendError = ''
          closeCompose()
        }}
        onsendfailed={onComposeSendFailed}
        onsentenqueued={onComposeSentEnqueued}
      />
    {/if}
  </div>
{/if}

<!-- "Respond with meeting" event editor — mounted at the app level
     so it can overlay any view. Driven entirely by `meetingDraft`:
     setting it opens the editor pre-filled (subject as title,
     From/To as required attendees, Cc as optional, auto-created
     Talk room), clearing it dismisses. -->
{#if meetingDraft}
  <EventEditor
    mode="create"
    calendars={meetingDraft.calendars}
    draft={meetingDraft.draft}
    onclose={onMeetingEditorClose}
    onsaved={onMeetingEditorSaved}
  />
{/if}

<!-- App-level right-click fallback menu (#198).  Mounted at the
     root so it floats over every view; only opens when no
     row-level oncontextmenu handler claimed the click. -->
{#if appContextMenu}
  <button
    type="button"
    class="fixed inset-0 z-40 cursor-default"
    aria-label="Close menu"
    onclick={closeAppContextMenu}
    onkeydown={(e) => e.key === 'Escape' && closeAppContextMenu()}
  ></button>
  <div
    role="menu"
    tabindex="-1"
    class="fixed z-50 min-w-44 rounded-md shadow-lg border border-surface-300 dark:border-surface-700 bg-surface-50 dark:bg-surface-900 py-1 text-sm"
    style="left: {appContextMenu.x}px; top: {appContextMenu.y}px"
    onmousedown={(e) => e.stopPropagation()}
  >
    <button
      type="button"
      role="menuitem"
      class="flex w-full items-center gap-2 text-left px-3 py-1.5 hover:bg-surface-200 dark:hover:bg-surface-800"
      onclick={reloadFrontendFromContextMenu}
    >
      <Icon name="sync" size={16} />
      <span>Refresh</span>
    </button>
  </div>
{/if}
