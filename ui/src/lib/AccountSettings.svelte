<script lang="ts">
  /**
   * AccountSettings — displays and manages configured email accounts.
   *
   * Shows a list of all accounts with options to remove them or
   * add new ones. This is accessible from the sidebar's settings
   * area and lets users manage their accounts after initial setup.
   */

  import { convertFileSrc, invoke } from '@tauri-apps/api/core'
  import { isNextcloudSource } from './ncSources'
  import { listen } from '@tauri-apps/api/event'
  import { open as openFileDialog } from '@tauri-apps/plugin-dialog'
  import { enable as autostartEnable, disable as autostartDisable, isEnabled as autostartIsEnabled } from '@tauri-apps/plugin-autostart'
  import NextcloudSettings from './NextcloudSettings.svelte'
  import SecuritySettings from './SecuritySettings.svelte'
  import EmojiPicker from './EmojiPicker.svelte'
  import Icon, { type IconName } from './Icon.svelte'
  import RichTextEditor from './RichTextEditor.svelte'
  import EncryptionSettings from './EncryptionSettings.svelte'
  import SmimeSettings from './SmimeSettings.svelte'
  import {
    openSignatureEditorInStandaloneWindow,
    SIGNATURE_POPOUT_CLOSED_EVENT,
    SIGNATURE_UPDATED_EVENT,
  } from './standaloneSignatureEditorWindow'
  import Toggle from './Toggle.svelte'
  import {
    STOCK_THEMES,
    applyTheme,
    type ThemeMode,
    type ThemeOption,
  } from './theme'
  import {
    applyUiScale,
    effectiveScale,
    MAX_UI_SCALE,
    MIN_UI_SCALE,
    UI_SCALE_STEP,
  } from './uiScale'
  import { m } from '../paraglide/messages'
  import { getLocale, locales } from '../paraglide/runtime'
  import {
    downloadBundle,
    getSyncState,
    ncProbeBundle,
    ncRestoreBundle,
    notifySettingsChanged,
    setSyncTarget,
    uploadBundle,
    type SettingsSyncStateView,
  } from './settingsBundle'
  import {
    listTrustedSenders,
    removeTrustedSender,
  } from './trustedSenders'

  // Friendly labels for the locale picker.  Keep the codes in
  // lockstep with `project.inlang/settings.json`.  Native
  // names (the language as written in *that* language) so the
  // user can find their language even when the UI is in a
  // language they don't read.
  const LOCALE_LABELS: Record<string, string> = {
    en: 'English',
    de: 'Deutsch',
  }

  /** Drives the "Restart required" prompt that appears after
   *  the user picks a new locale (#190).  Paraglide reads its
   *  active locale exactly once at process start (from the
   *  localStorage pin → navigator.language → baseLocale chain),
   *  so a runtime switch can't fully take effect without a
   *  fresh process.  Showing the prompt lets the user decide
   *  whether to relaunch immediately or wait until next time
   *  they open the app. */
  let restartPromptOpen = $state(false)
  /** Snapshot of the locale that's active *right now* in the
   *  running process.  We compare against this when deciding
   *  whether to surface the restart prompt — picking the
   *  language that's already running is a no-op and shouldn't
   *  nag the user. */
  const initialLocale = getLocale()

  // ── Types ───────────────────────────────────────────────────
  // Mirrors the Rust `Account` struct from unkai-core
  interface FolderIconRule {
    keyword: string
    icon: string
  }
  interface Account {
    id: string
    display_name: string
    email: string
    imap_host: string
    imap_port: number
    smtp_host: string
    smtp_port: number
    use_jmap: boolean
    jmap_url?: string | null
    signature?: string | null
    folder_icons?: FolderIconRule[]
    /** Per-account TLS trust list. Each entry is a leaf cert the
     *  user has explicitly trusted via the AccountSetup wizard or
     *  the Re-trust button below. Round-tripped through
     *  `update_account` whenever the trust list changes (e.g.
     *  after a server cert renewal). */
    trusted_certs?: TrustedCert[]
    folder_icon_overrides?: Record<string, string>
    /** Optional emoji avatar for the IconRail (#115). */
    emoji?: string | null
    /** Display order in the IconRail; lower = top. */
    sort_order?: number
    /** Human's full name for outbound From: header (#115). */
    person_name?: string | null
    /** Uppercase hex fingerprint of the account's imported OpenPGP
     *  private key (#57).  Display-only — the armored key itself
     *  lives in the OS keychain.  `null` / undefined means no key
     *  imported; the AccountSettings encryption section then
     *  surfaces the "Import private key" affordance. */
    pgp_key_fingerprint?: string | null
  }

  interface TrustedCert {
    /** DER bytes as a JSON byte-array — matches the Rust
     *  `Vec<u8>` serialisation. */
    der: number[]
    /** SHA-256 fingerprint, lowercase hex with `:` separators. */
    sha256: string
    host: string
    /** Unix epoch seconds when the cert was trusted. */
    added_at: number
  }

  // ── Category navigation (#131) ──────────────────────────────
  // Settings used to be one long scroll; #131 split it into the
  // categories users actually look for.  The nav lives in a
  // left column and `activeCategory` gates which section block
  // renders so each panel stays focused.
  export type SettingsCategory =
    | 'general'
    | 'design'
    | 'mail'
    | 'calendar'
    | 'nextcloud'
    | 'security'
    | 'backup'

  // ── Props ───────────────────────────────────────────────────
  interface Props {
    onclose: () => void         // Go back to the inbox view
    onaddaccount: () => void    // Switch to the setup wizard to add another account
    /** Notify the parent (App.svelte) whenever app-wide preferences
        change so it can keep its cached snapshot — and the theme
        `$effect` it drives — in sync. Optional so callers that don't
        care about live updates aren't forced to handle it. */
    onappprefschanged?: (prefs: AppSettings) => void
    /** Notify the parent when the connected Nextcloud accounts (or
        their capabilities) change so the IconRail can refresh its
        integration icons immediately, without waiting for the
        Settings panel to close (#318). */
    onnextcloudchanged?: () => void
    /** Restart the guided welcome tour (#420).  The parent owns the
        tour overlay (it spans the whole shell, not just Settings)
        and switches back to the inbox first so the tour's anchors
        are mounted. */
    onreplaytour?: () => void
    /** Which settings category the panel is currently showing.
        Lifted to the parent (#318) so that a remount of Settings
        — e.g. the user clicks a freshly-revealed Nextcloud
        integration icon, looks at it, then comes back to Settings
        — preserves the category instead of snapping to General. */
    activeCategory?: SettingsCategory
  }
  let {
    onclose,
    onaddaccount,
    onappprefschanged,
    onnextcloudchanged,
    onreplaytour,
    activeCategory = $bindable('general'),
  }: Props = $props()
  interface CategoryEntry {
    id: SettingsCategory
    label: string
    icon: IconName
  }
  // `cloud` from set v3 covers Nextcloud cleanly; Design still
  // borrows `share-links` since the family has no
  // theme/palette glyph yet.
  const CATEGORIES: CategoryEntry[] = [
    { id: 'general', label: 'General', icon: 'settings' },
    { id: 'design', label: 'Design', icon: 'design-palette' },
    { id: 'mail', label: 'E-Mail', icon: 'email-envelope' },
    { id: 'calendar', label: 'Calendar', icon: 'calendar' },
    { id: 'nextcloud', label: 'Nextcloud', icon: 'cloud' },
    { id: 'security', label: 'Security', icon: 'lock' },
    // #168 — the page where the user manages local export +
    // Nextcloud-recovery sync.  The `sync` glyph (two arrows in a
    // loop) communicates "this is the page where state moves
    // back and forth between machines" better than a generic
    // archive icon.
    { id: 'backup', label: 'Backup & Sync', icon: 'sync' },
  ]

  // ── State ───────────────────────────────────────────────────
  let accounts = $state<Account[]>([])
  let loading = $state(true)
  let error = $state('')

  // ── App-wide preferences (Issue #16) ────────────────────────
  // Mirrors the Rust `AppSettings` struct. A missing/failing load
  // falls back to the Rust-side defaults — we never render with a
  // null form state.
  interface AppSettings {
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
    /** #334 — when off, the MailList renders every envelope as
     *  its own row instead of bundling reply chains under one
     *  conversation head with an expand chevron.  Default on. */
    conversation_view_enabled: boolean
    /** #165 master toggle for the URLhaus link checker.
     *  Off → no pills, no click interception, refresh worker
     *  sleeps.  On (default) → links in rendered email show
     *  green / red safety pills and unsafe clicks confirm.
     *  Always present once `loadAppSettings` returns (the Rust
     *  side serialises it with a default), so the type is
     *  required even though older bundle files may lack it. */
    link_check_enabled: boolean
    default_calendar_id: string | null
    /** Reminders for events that carry a meeting URL (Talk /
     *  Zoom / Meet / Teams / Jitsi / …).  #203 renamed from
     *  `talk_reminder_enabled`; backend keeps the old name as
     *  a serde alias so existing user configs still load. */
    meeting_reminders_enabled: boolean
    /** Reminders for events *without* a meeting URL (#203).
     *  Independent toggle so the two streams can be muted
     *  separately. */
    calendar_reminders_enabled: boolean
    /** When true, a meeting reminder firing at "now" (≤1 min
     *  lead) opens the meeting URL straight away instead of
     *  showing the popup.  Off by default (#203 follow-up). */
    auto_open_meetings: boolean
    autostart_enabled: boolean
    /** User-imported Skeleton themes (#132 tier 2). */
    custom_themes?: CustomThemeRow[]
    /** App-icon style slug — drives the tray, window titlebar and
     *  taskbar icon. Picker lives in Settings → Design. */
    logo_style?: string
    /** #191 manual UI-scale (0.7 .. 1.5). */
    ui_scale?: number
    /** #191 when true, scale is auto-derived from screen width. */
    ui_scale_auto?: boolean
    /** #190 BCP-47 locale tag for the display language. */
    ui_locale?: string
    /** #190 when true, the locale follows `navigator.language`. */
    ui_locale_auto?: boolean
    /** #280 master toggle for the EventEditor's location
     *  autocomplete + inline map preview.  Off (default) keeps
     *  Location as a plain text input that never leaves the
     *  device; on opts the user into Nominatim queries and
     *  OpenStreetMap tile loads.  Non-optional because the Rust
     *  side serialises a default value on `get_app_settings`. */
    location_geocoding_enabled: boolean
    /** #259 follow-up — override URL for forward-geocoding.
     *  Empty string = use the public default
     *  (`https://nominatim.openstreetmap.org`).  Self-hosters
     *  can point this at their own Nominatim instance. */
    nominatim_base_url: string
    /** #260 — when true, a `mail://` click in a note routes
     *  through the main view-switch so the inbox lands on that
     *  message.  When false (default) the click opens a
     *  standalone reader window and the Notes view stays put.
     *  Optional because older settings bundles won't carry the
     *  key. */
    notes_mail_open_in_view?: boolean
    /** #416 — response policy for incoming read-receipt requests
     *  (RFC 8098): `'never'` suppresses the banner and sends
     *  nothing, `'ask'` (default) prompts per message, `'always'`
     *  sends automatically on display.  Non-optional because the
     *  Rust side serialises a default. */
    mdn_response_mode: 'never' | 'ask' | 'always'
  }
  interface CustomThemeRow {
    id: string
    label: string
    description?: string
    path: string
  }

  let appSettings = $state<AppSettings>({
    minimize_to_tray: true,
    background_sync_enabled: true,
    background_sync_interval_secs: 300,
    notifications_enabled: true,
    start_minimized: false,
    theme_name: 'cerberus',
    theme_mode: 'system',
    mail_html_white_background: true,
    auto_load_remote_images: false,
    auto_advance_after_remove: true,
    conversation_view_enabled: true,
    link_check_enabled: true,
    default_calendar_id: null,
    meeting_reminders_enabled: true,
    calendar_reminders_enabled: true,
    auto_open_meetings: false,
    autostart_enabled: false,
    custom_themes: [],
    logo_style: 'storm',
    ui_scale: 1.0,
    ui_scale_auto: true,
    ui_locale: '',
    ui_locale_auto: true,
    location_geocoding_enabled: false,
    nominatim_base_url: '',
    notes_mail_open_in_view: false,
    mdn_response_mode: 'ask',
  })

  /** The default Nominatim base URL — surfaced in the settings
   *  field's placeholder and as the value the Reset button
   *  restores.  Mirrors `geocode::DEFAULT_NOMINATIM_BASE_URL`
   *  on the Rust side.  We keep the constant local rather than
   *  fetching it via IPC because it's stable, public, and
   *  changing it would be a breaking change in any case. */
  const DEFAULT_NOMINATIM_BASE_URL = 'https://nominatim.openstreetmap.org'

  // ── Trusted senders (#295) ──────────────────────────────────
  // Mirrors the localStorage allow-list maintained by
  // `trustedSenders.ts`.  Loaded once on mount and refreshed
  // after each remove — the list is small (typically a handful
  // of entries) so a full re-read is cheaper than threading a
  // subscription through.
  let trustedSenders = $state<string[]>([])
  $effect(() => {
    trustedSenders = listTrustedSenders()
  })
  function onRemoveTrustedSender(addr: string) {
    removeTrustedSender(addr)
    trustedSenders = listTrustedSenders()
  }

  // ── Logo / app-icon picker (Issue #X) ───────────────────────
  // Each entry is one slug the Rust side knows: the image source
  // is the `unkai-logo://localhost/<id>` URI scheme (registered
  // in main.rs) which streams the embedded PNG bytes back as a
  // plain image response. Click → invoke `set_logo_style`, which
  // hot-swaps the running tray + window icon and persists the
  // pick to `app_settings.json`.
  interface LogoStyle {
    id: string
    label: string
    swatchTone: string  // bg-tailwind class for the underline accent
  }
  const LOGO_STYLES: LogoStyle[] = [
    // v1 — atmospheric set
    { id: 'storm',             label: 'Storm',            swatchTone: 'bg-blue-500' },
    { id: 'dawn',              label: 'Dawn',             swatchTone: 'bg-orange-400' },
    { id: 'mint',              label: 'Mint',             swatchTone: 'bg-emerald-400' },
    { id: 'sky',               label: 'Sky',              swatchTone: 'bg-sky-400' },
    { id: 'twilight',          label: 'Twilight',         swatchTone: 'bg-violet-500' },
    { id: 'monochrome-black',  label: 'Mono — Black',     swatchTone: 'bg-slate-900' },
    { id: 'monochrome-white',  label: 'Mono — White',     swatchTone: 'bg-slate-200' },
    // v2 — elemental set
    { id: 'copper',            label: 'Copper',           swatchTone: 'bg-orange-700' },
    { id: 'forest',            label: 'Forest',           swatchTone: 'bg-green-700' },
    { id: 'midnight',          label: 'Midnight',         swatchTone: 'bg-indigo-900' },
    { id: 'ocean',             label: 'Ocean',            swatchTone: 'bg-cyan-600' },
    { id: 'rose',              label: 'Rose',             swatchTone: 'bg-rose-500' },
    { id: 'slate',             label: 'Slate',            swatchTone: 'bg-slate-500' },
    { id: 'sunset',            label: 'Sunset',           swatchTone: 'bg-orange-500' },
  ]
  let logoSaving = $state(false)

  async function pickLogoStyle(id: string) {
    if (logoSaving || appSettings.logo_style === id) return
    logoSaving = true
    try {
      await invoke('set_logo_style', { style: id })
      appSettings.logo_style = id
      onappprefschanged?.({ ...appSettings })
    } catch (e) {
      console.warn('set_logo_style failed', e)
    } finally {
      logoSaving = false
    }
  }

  /** Picker rows — stock themes plus the user's imports.  Driven
   *  by `appSettings.custom_themes` so a fresh import / remove
   *  triggers Svelte's reactivity automatically (the previous
   *  Proxy-based `THEMES` export couldn't, because it was a
   *  plain module-level mutable). */
  const pickerThemes = $derived<ThemeOption[]>([
    ...STOCK_THEMES,
    ...((appSettings.custom_themes ?? []).map((t) => ({
      id: t.id,
      label: t.label,
      description: t.description ?? 'Imported theme',
      custom: true,
    }))),
  ])

  // Calendar list for the "default calendar" picker.  Loaded
  // lazily once on mount alongside the other app settings.
  // Empty list = no Nextcloud connected yet → setting is hidden.
  interface CalendarRow {
    id: string
    nextcloud_account_id: string
    display_name: string
    color: string | null
    last_synced_at: string | null
    hidden?: boolean
    /** CalDAV-derived read-only flag (#236).  The default-
     *  calendar picker still lists these so the user can pick
     *  a shared calendar as their default, but EventEditor's
     *  create-mode picker filters them out and falls back to
     *  the first writable calendar. */
    read_only?: boolean
  }
  let calendarsForPicker = $state<CalendarRow[]>([])
  let prefsSaveStatus = $state<'' | 'saving' | 'saved' | 'error'>('')
  let checkNowBusy = $state(false)

  // ── Link-check status (#165) ─────────────────────────────────
  // Surfaced under Settings → E-Mail next to the toggle.  Two
  // pieces of state: the snapshot status (count + last refresh
  // timestamp) and a busy flag for the manual "Refresh now"
  // button so it can show a spinner-ish disabled state.
  interface LinkCheckStatus {
    totalUrls: number
    lastRefreshedAt: string | null
  }
  let linkCheckStatus = $state<LinkCheckStatus>({ totalUrls: 0, lastRefreshedAt: null })
  let linkCheckRefreshing = $state(false)
  async function loadLinkCheckStatus() {
    try {
      linkCheckStatus = await invoke<LinkCheckStatus>('get_link_check_status')
    } catch (e) {
      console.warn('get_link_check_status failed', e)
    }
  }
  async function onRefreshUrlhausNow() {
    if (linkCheckRefreshing) return
    linkCheckRefreshing = true
    try {
      await invoke('refresh_urlhaus_now')
      await loadLinkCheckStatus()
    } catch (e) {
      console.warn('refresh_urlhaus_now failed', e)
    } finally {
      linkCheckRefreshing = false
    }
  }
  function formatRefreshAge(ts: string | null): string {
    if (!ts) return 'Never'
    const then = new Date(ts).getTime()
    if (!Number.isFinite(then)) return 'Unknown'
    const ageSec = Math.max(0, (Date.now() - then) / 1000)
    if (ageSec < 60) return 'Just now'
    if (ageSec < 3600) return `${Math.floor(ageSec / 60)} min ago`
    if (ageSec < 86_400) return `${Math.floor(ageSec / 3600)} h ago`
    return `${Math.floor(ageSec / 86_400)} d ago`
  }

  // ── Backup & Sync (#168) ─────────────────────────────────────
  // State for the Backup & Sync category panel.  The dropdown
  // shows every connected NC account so the user can pick which
  // one becomes the recovery destination, mirrored by a per-row
  // toggle on the Nextcloud category.
  interface NcOption {
    id: string
    label: string
  }
  let ncOptions = $state<NcOption[]>([])
  let syncState = $state<SettingsSyncStateView>({ targetNcId: null, pending: false })
  /** Status string surfaced under the Download / Upload buttons.
   *  Mirrors the `prefsSaveStatus` pattern but with longer-lived
   *  messages (we want the user to see "Saved to /tmp/foo.json"
   *  for a few seconds, not flash through "saved" → ""). */
  let backupStatus = $state<{ kind: 'idle' | 'busy' | 'done' | 'error'; msg: string }>(
    { kind: 'idle', msg: '' },
  )
  function showBackupStatus(kind: 'busy' | 'done' | 'error', msg: string) {
    backupStatus = { kind, msg }
    if (kind !== 'busy') {
      setTimeout(() => {
        if (backupStatus.kind === kind && backupStatus.msg === msg) {
          backupStatus = { kind: 'idle', msg: '' }
        }
      }, 4000)
    }
  }

  async function loadSyncState() {
    try {
      syncState = await getSyncState()
    } catch (e) {
      console.warn('get_settings_sync_state failed', e)
    }
  }
  async function loadNcOptions() {
    try {
      const accs = await invoke<{ id: string; username: string; server_url: string; display_name?: string | null; kind?: string }[]>('get_nextcloud_accounts')
      // The settings bundle lives on Nextcloud WebDAV — generic-DAV
      // and local sources (#413) can't be backup targets.
      ncOptions = accs
        .filter(isNextcloudSource)
        .map((a) => ({
          id: a.id,
          label: a.display_name || `${a.username} @ ${new URL(a.server_url).hostname}`,
        }))
    } catch (e) {
      console.warn('loadNcOptions failed', e)
      ncOptions = []
    }
  }
  async function onSyncTargetChange(next: string | null) {
    try {
      await setSyncTarget(next)
      await loadSyncState()
    } catch (e) {
      console.warn('setSyncTarget failed', e)
    }
  }
  async function onDownloadClick() {
    showBackupStatus('busy', 'Saving…')
    try {
      const path = await downloadBundle()
      if (path) showBackupStatus('done', `Saved to ${path}`)
      else backupStatus = { kind: 'idle', msg: '' }
    } catch (e: any) {
      showBackupStatus('error', e?.message ?? String(e))
    }
  }
  async function onUploadClick() {
    if (
      !confirm(
        'Importing a settings backup will overwrite your current preferences and merge in any accounts from the file. Continue?',
      )
    )
      return
    showBackupStatus('busy', 'Importing…')
    try {
      const path = await uploadBundle()
      if (path) {
        showBackupStatus('done', 'Imported — reload the window to see every change.')
        await loadAppSettings()
        await loadAccounts()
      } else {
        backupStatus = { kind: 'idle', msg: '' }
      }
    } catch (e: any) {
      showBackupStatus('error', e?.message ?? String(e))
    }
  }
  async function onRestoreFromNcClick() {
    if (!syncState.targetNcId) return
    if (
      !confirm(
        'Replace your local preferences with the latest backup from this Nextcloud? Existing accounts on this machine are kept; the bundle just adds back any that were missing and updates metadata.',
      )
    )
      return
    showBackupStatus('busy', 'Downloading from Nextcloud…')
    try {
      await ncRestoreBundle(syncState.targetNcId)
      showBackupStatus('done', 'Restored — reload the window to see every change.')
      await loadAppSettings()
      await loadAccounts()
    } catch (e: any) {
      showBackupStatus('error', e?.message ?? String(e))
    }
  }

  // ── Load accounts on mount ──────────────────────────────────
  // $effect runs when the component is first rendered (like onMount).
  // We call the Rust backend to get all saved accounts.
  $effect(() => {
    loadAccounts()
    loadAppSettings()
    loadCalendarsForPicker()
    loadSyncState()
    loadNcOptions()
    loadLinkCheckStatus()
  })

  async function loadCalendarsForPicker() {
    try {
      const accounts = await invoke<{ id: string }[]>('get_nextcloud_accounts')
      const all: CalendarRow[] = []
      for (const acc of accounts) {
        try {
          const cs = await invoke<CalendarRow[]>('get_cached_calendars', {
            ncId: acc.id,
          })
          all.push(...cs)
        } catch (e) {
          console.warn('default-calendar picker: get_cached_calendars failed', e)
        }
      }
      calendarsForPicker = all.filter((c) => !c.hidden)
    } catch (e) {
      console.warn('default-calendar picker: get_nextcloud_accounts failed', e)
      calendarsForPicker = []
    }
  }

  async function loadAppSettings() {
    try {
      appSettings = await invoke<AppSettings>('get_app_settings')
    } catch (e: any) {
      console.warn('failed to load app settings', e)
    }
  }

  // Debounced save so dragging the interval field doesn't hammer the
  // disk on every keystroke. 400 ms is imperceptible to the user but
  // easily coalesces a burst of edits.
  let saveTimer: ReturnType<typeof setTimeout> | null = null
  function scheduleSave() {
    prefsSaveStatus = 'saving'
    // Tell the parent immediately so its derived state (notification
    // toggle, theme `$effect`) reacts without waiting for the debounce.
    onappprefschanged?.({ ...appSettings })
    if (saveTimer) clearTimeout(saveTimer)
    saveTimer = setTimeout(async () => {
      try {
        await invoke('update_app_settings', { newSettings: appSettings })
        prefsSaveStatus = 'saved'
        // Settings changed → kick the auto-sync worker so a
        // recovery push to the configured Nextcloud (#168)
        // happens in the background.  Fire-and-forget; the
        // helper logs and swallows its own errors.
        void notifySettingsChanged()
        setTimeout(() => {
          if (prefsSaveStatus === 'saved') prefsSaveStatus = ''
        }, 1500)
      } catch (e: any) {
        console.warn('failed to save app settings', e)
        prefsSaveStatus = 'error'
      }
    }, 400)
  }

  /** Toggle "launch on login" via the autostart plugin and
   *  persist the user's choice.  We talk to the OS first
   *  because the plugin call is the cross-platform side-effect
   *  (XDG entry / LaunchAgent / registry key); only on its
   *  success do we commit the new bit to AppSettings.  That
   *  way a misconfigured environment (e.g. read-only
   *  ~/.config) can't leave us with a checked box that doesn't
   *  actually autostart. */
  async function onAutostartToggle(next: boolean) {
    try {
      if (next) await autostartEnable()
      else await autostartDisable()
      appSettings.autostart_enabled = next
      scheduleSave()
    } catch (e) {
      console.warn('autostart toggle failed', e)
      // Roll the checkbox state back so the UI matches reality.
      appSettings.autostart_enabled = !next
      prefsSaveStatus = 'error'
    }
  }

  /** Open the OS's default-apps settings page so the user can
   *  pick Unkai as the default handler for .ics / .eml (#254).
   *  Unkai is *eligible* once installed thanks to the
   *  `bundle.fileAssociations` declarations, but on Windows
   *  marking it as the *default* requires user consent in the
   *  Settings panel — programmatic association without the
   *  user clicking through is locked down by Windows for
   *  anti-hijacking reasons.  The Rust command shell-execs the
   *  appropriate OS surface (`ms-settings:defaultapps` /
   *  `~/Library/Preferences` / xdg-mime hint). */
  async function openDefaultAppsSettings() {
    try {
      await invoke('open_default_apps_settings')
    } catch (e) {
      alert(`Could not open the default-apps settings page: ${e}`)
    }
  }

  /** Reconcile the stored bit against the OS on mount — picks
   *  up the case where the user removed the autostart entry
   *  manually (e.g. via system settings) since the last
   *  launch. */
  $effect(() => {
    void autostartIsEnabled()
      .then((enabled) => {
        if (enabled !== appSettings.autostart_enabled) {
          appSettings.autostart_enabled = enabled
        }
      })
      .catch((e) => console.warn('autostart isEnabled failed', e))
  })

  // ── Custom theme import (#132 tier 2) ──────────────────────
  // The picker shows an "Import theme…" button next to the
  // Theme heading.  Clicking it opens a native file dialog
  // restricted to `.css` files; the picked path is handed to
  // the backend's `import_custom_theme` IPC, which copies the
  // bytes into the app's themes dir, parses out the
  // `[data-theme="…"]` slug, and persists a `CustomTheme` row.
  // App.svelte's `custom-themes-changed` listener picks the
  // change up and re-seeds the theme module's runtime registry,
  // so the new entry appears in the picker without a reload.
  let importingTheme = $state(false)
  async function importCustomTheme() {
    if (importingTheme) return
    importingTheme = true
    try {
      const picked = await openFileDialog({
        multiple: false,
        directory: false,
        filters: [{ name: 'CSS theme', extensions: ['css'] }],
      })
      if (!picked) return
      // tauri-plugin-dialog returns the path as a plain string when
      // `multiple: false, directory: false` is set.
      const path = picked
      const fileName = path.split(/[\\/]/).pop() ?? ''
      const stem = fileName.replace(/\.css$/i, '')
      // Reasonable default label — the user can rename later.
      const label = stem.replace(/[_-]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase())
      await invoke('import_custom_theme', {
        sourcePath: path,
        label,
      })
      // Re-pull the live settings snapshot — the import added
      // a row to `custom_themes`, and the picker reads it via
      // `pickerThemes` $derived.  Without this refresh the new
      // theme would only appear after a full reload.
      await reloadSettingsSnapshot()
    } catch (e) {
      console.warn('import_custom_theme failed', e)
    } finally {
      importingTheme = false
    }
  }
  async function removeCustomTheme(id: string) {
    if (!confirm('Remove this custom theme? The CSS file in the app data folder will be deleted.')) {
      return
    }
    try {
      await invoke('remove_custom_theme', { id })
      await reloadSettingsSnapshot()
    } catch (e) {
      console.warn('remove_custom_theme failed', e)
    }
  }
  /** Pull the just-saved AppSettings back into `appSettings`
   *  so derived state (the picker list, the From: header, …)
   *  recomputes against the new server-side truth without a
   *  full page reload. */
  async function reloadSettingsSnapshot() {
    try {
      const fresh = await invoke<AppSettings>('get_app_settings')
      appSettings = fresh
      onappprefschanged?.({ ...fresh })
    } catch (e) {
      console.warn('get_app_settings refresh failed', e)
    }
  }

  /** Theme picker handler — apply the change to the DOM immediately
      so the user sees it before the debounced save fires. We still go
      through `scheduleSave` so the persistence + parent-notify path
      is unchanged. */
  function onThemeChange(name: string, mode: ThemeMode) {
    appSettings.theme_name = name
    appSettings.theme_mode = mode
    applyTheme(name, mode)
    scheduleSave()
  }

  async function runCheckMailNow() {
    checkNowBusy = true
    try {
      await invoke('check_mail_now')
    } catch (e: any) {
      console.warn('check_mail_now failed', e)
    } finally {
      checkNowBusy = false
    }
  }

  async function loadAccounts() {
    loading = true
    error = ''
    try {
      accounts = await invoke<Account[]>('get_accounts')
    } catch (e: any) {
      error = typeof e === 'string' ? e : e?.message ?? 'Failed to load accounts'
    } finally {
      loading = false
    }
  }

  async function removeAccount(id: string, email: string) {
    // Simple confirmation — in a real app you might use a modal
    if (!confirm(`Remove account ${email}? This cannot be undone.`)) return

    try {
      await invoke('remove_account', { id })
      // Refresh the list after removal
      await loadAccounts()
    } catch (e: any) {
      error = typeof e === 'string' ? e : e?.message ?? 'Failed to remove account'
    }
  }

  // ── TLS re-trust flow ───────────────────────────────────────
  //
  // The AccountSetup wizard already trusts a self-signed leaf at
  // the moment the account is added, but the trust list is frozen
  // after that. When the user's mail server rotates its cert
  // (Let's-Encrypt-style renewals on a self-signed CA, manual
  // re-issuance, etc.) every IMAP/SMTP connect bombs with
  // "invalid peer certificate: UnknownIssuer" and there's no in-
  // app way to recover — the user's stuck reading nothing.
  //
  // This pair of helpers re-runs the same probe-and-trust dance
  // the wizard uses, against an account that already exists. The
  // probe captures the *full chain* (leaf + intermediates) the
  // server is currently presenting, the user reviews each
  // fingerprint, and on confirm every cert in the chain is
  // appended to `account.trusted_certs`. Trusting the whole chain
  // (not just the leaf) means a future leaf reissue under the
  // same intermediate, or a server that reorders certs on the
  // wire, still resolves through the verifier's chain-walk
  // matcher without dropping the user back into this prompt.

  interface ProbedCertEntry {
    der: number[]
    sha256: string
  }
  interface ProbedCert {
    chain: ProbedCertEntry[]
    host: string
  }

  /** Probed chain awaiting confirmation. `null` = no flow open. */
  let trustPrompt = $state<{
    account: Account
    chain: ProbedCertEntry[]
  } | null>(null)
  let trustBusy = $state(false)
  let trustError = $state('')

  async function startRetrust(account: Account) {
    trustError = ''
    trustBusy = true
    try {
      const probed = await invoke<ProbedCert>('probe_server_certificate', {
        host: account.imap_host,
        port: account.imap_port,
      })
      trustPrompt = {
        account,
        chain: probed.chain,
      }
    } catch (e: any) {
      trustError = typeof e === 'string' ? e : e?.message ?? 'Failed to probe server certificate'
    } finally {
      trustBusy = false
    }
  }

  async function commitRetrust() {
    if (!trustPrompt || trustBusy) return
    trustBusy = true
    trustError = ''
    try {
      // Append every cert in the probed chain to the account's
      // trust list. We don't dedupe — `unkai_core::tls::
      // build_client_config` happily accepts duplicates, and an
      // exact-match dupe is harmless. Anything else (cert renewed
      // under same CN, server moved hosts, …) is a *new* entry
      // the user explicitly wants.
      const addedAt = Math.floor(Date.now() / 1000)
      const additions: TrustedCert[] = trustPrompt.chain.map((entry) => ({
        der: entry.der,
        sha256: entry.sha256,
        host: trustPrompt!.account.imap_host,
        added_at: addedAt,
      }))
      const updated: Account = {
        ...trustPrompt.account,
        trusted_certs: [...(trustPrompt.account.trusted_certs ?? []), ...additions],
      }
      await invoke('update_account', { account: updated })
      trustPrompt = null
      await loadAccounts()
    } catch (e: any) {
      trustError = typeof e === 'string' ? e : e?.message ?? 'Failed to update account'
    } finally {
      trustBusy = false
    }
  }

  function cancelRetrust() {
    trustPrompt = null
    trustError = ''
  }

  // ── Signature editing ───────────────────────────────────────
  // The signature lives directly on the `Account` row. Edits are
  // debounced like the app preferences so dragging through a long
  // textarea doesn't write to disk on every keystroke.
  const sigSaveTimers = new Map<string, ReturnType<typeof setTimeout>>()
  const sigSaveStatus = $state<Record<string, '' | 'saving' | 'saved' | 'error'>>({})

  function onSignatureChange(account: Account, next: string) {
    account.signature = next
    sigSaveStatus[account.id] = 'saving'
    const existing = sigSaveTimers.get(account.id)
    if (existing) clearTimeout(existing)
    sigSaveTimers.set(
      account.id,
      setTimeout(async () => {
        try {
          // The Rust `update_account` takes the full Account record;
          // sending the in-place edited copy is fine because we never
          // mutate fields the user can't edit here (host/port/etc).
          await invoke('update_account', {
            account: { ...account, signature: next.trim() || null },
          })
          sigSaveStatus[account.id] = 'saved'
          setTimeout(() => {
            if (sigSaveStatus[account.id] === 'saved') sigSaveStatus[account.id] = ''
          }, 1500)
        } catch (e) {
          console.warn('failed to save signature', e)
          sigSaveStatus[account.id] = 'error'
        }
      }, 400),
    )
  }

  // #314 — popped-out signature editor.  Tracks which accounts
  // currently have a popout window open so we can lock the inline
  // editor (the user otherwise sees two live editors fighting over
  // the same `account.signature`).  Keyed by account id; the popout
  // emits `signature-popout-closed` on close, which clears the flag
  // and re-enables the inline editor.
  const sigPopoutOpen = $state<Record<string, boolean>>({})

  function openSignaturePopout(account: Account) {
    if (sigPopoutOpen[account.id]) return
    sigPopoutOpen[account.id] = true
    void openSignatureEditorInStandaloneWindow({
      accountId: account.id,
      accountLabel:
        account.display_name?.trim() || account.email || account.id,
      initialHtml: account.signature ?? '',
    }).catch((e) => {
      console.warn('openSignatureEditorInStandaloneWindow failed', e)
      // Roll back the lock so the user can retry — keeping it set
      // would leave the inline editor permanently disabled for a
      // window that never actually opened.
      sigPopoutOpen[account.id] = false
    })
  }

  // Listen for save / close events emitted by the popped-out window.
  // Save events keep the inline `account.signature` in sync (so when
  // the user closes the popout the locked editor flashes the final
  // content rather than the stale launch-time snapshot); close events
  // re-enable the inline editor.
  $effect(() => {
    const unlisten: Array<Promise<() => void>> = []

    unlisten.push(
      listen<{ accountId: string; html: string }>(
        SIGNATURE_UPDATED_EVENT,
        (event) => {
          const acc = accounts.find((a) => a.id === event.payload.accountId)
          if (!acc) return
          acc.signature = event.payload.html.trim() || null
        },
      ),
    )
    unlisten.push(
      listen<{ accountId: string }>(SIGNATURE_POPOUT_CLOSED_EVENT, (event) => {
        sigPopoutOpen[event.payload.accountId] = false
      }),
    )

    return () => {
      for (const p of unlisten) {
        void p.then((fn) => fn()).catch(() => {})
      }
    }
  })

  // ── Custom folder icons (Issue #63) ─────────────────────────
  // Each account carries a list of `{keyword, icon}` rules. The
  // sidebar applies them before its built-in icon heuristics. Edits
  // save immediately (no debounce) — the dataset is tiny and each
  // change is the result of an explicit click, not keystroke spam.
  const iconSaveStatus = $state<Record<string, '' | 'saving' | 'saved' | 'error'>>({})
  // svelte-ignore state_referenced_locally
  const iconDrafts = $state<Record<string, { keyword: string; icon: string }>>({})

  /** Make sure every account has a stable draft slot before the
      template tries to `bind:` to it — `bind:` requires a plain
      MemberExpression, so the slot has to exist up-front. Runs as
      an `$effect` so it covers both initial load and any later
      account additions. */
  $effect(() => {
    for (const a of accounts) {
      if (!iconDrafts[a.id]) iconDrafts[a.id] = { keyword: '', icon: '' }
    }
  })

  /** Persist the emoji avatar (#115).  `null` clears it back to
   *  the initials fallback in the IconRail. */
  async function onEmojiChange(account: Account, next: string | null) {
    account.emoji = next
    try {
      await invoke('update_account', { account: { ...account, emoji: next } })
    } catch (e) {
      console.warn('failed to save account emoji', e)
    }
  }
  /** Which account currently has its emoji picker popover open. */
  let emojiPickerForAccount = $state<string | null>(null)
  /** Which account currently has its folder-icon-rule draft
   *  picker open (separate from the avatar picker — both can
   *  coexist on the same account row). */
  let iconDraftPickerFor = $state<string | null>(null)
  $effect(() => {
    if (!iconDraftPickerFor) return
    const onDoc = () => (iconDraftPickerFor = null)
    const handle = setTimeout(() => document.addEventListener('mousedown', onDoc), 0)
    return () => {
      clearTimeout(handle)
      document.removeEventListener('mousedown', onDoc)
    }
  })
  $effect(() => {
    if (!emojiPickerForAccount) return
    const onDoc = () => (emojiPickerForAccount = null)
    const handle = setTimeout(() => document.addEventListener('mousedown', onDoc), 0)
    return () => {
      clearTimeout(handle)
      document.removeEventListener('mousedown', onDoc)
    }
  })

  // ── Editable server settings ────────────────────────────────
  // Inline edit form for IMAP/SMTP host + port + password.
  // Expanded per-account; closed by default so the settings list
  // stays compact.  Drafts are kept in a local map so the user can
  // tweak fields without writing to the Account struct (which
  // would mutate the row above the form).
  interface ServerDraft {
    imap_host: string
    imap_port: number
    smtp_host: string
    smtp_port: number
  }
  // The modal renders against this account; null = closed.
  let serverEditAccount = $state<Account | null>(null)
  let serverDrafts = $state<Record<string, ServerDraft>>({})
  let serverSaveStatus = $state<Record<string, '' | 'saving' | 'saved' | 'error'>>({})
  let passwordDrafts = $state<Record<string, string>>({})

  function openServerEdit(account: Account) {
    serverDrafts[account.id] = {
      imap_host: account.imap_host,
      imap_port: account.imap_port,
      smtp_host: account.smtp_host,
      smtp_port: account.smtp_port,
    }
    passwordDrafts[account.id] = ''
    serverSaveStatus[account.id] = ''
    serverEditAccount = account
  }

  function closeServerEdit() {
    serverEditAccount = null
  }

  // One-shot save: persists hosts/ports through `update_account`
  // and, if the password field is non-empty, rotates the keychain
  // entry through `set_account_password`.  An empty password leaves
  // the existing keychain entry untouched, so the modal doubles as
  // "edit servers only" without forcing the user to retype.
  async function saveConnectionSettings(account: Account) {
    const draft = serverDrafts[account.id]
    if (!draft) return
    if (!draft.imap_host.trim() || !draft.smtp_host.trim()) {
      serverSaveStatus[account.id] = 'error'
      return
    }
    serverSaveStatus[account.id] = 'saving'
    try {
      const updated: Account = {
        ...account,
        imap_host: draft.imap_host.trim(),
        imap_port: draft.imap_port,
        smtp_host: draft.smtp_host.trim(),
        smtp_port: draft.smtp_port,
      }
      await invoke('update_account', { account: updated })
      Object.assign(account, updated)

      const newPassword = passwordDrafts[account.id] ?? ''
      if (newPassword) {
        await invoke('set_account_password', { id: account.id, password: newPassword })
        passwordDrafts[account.id] = ''
      }

      serverSaveStatus[account.id] = 'saved'
      setTimeout(() => {
        if (serverSaveStatus[account.id] === 'saved') serverSaveStatus[account.id] = ''
      }, 1500)
    } catch (e) {
      console.warn('failed to save connection settings', e)
      serverSaveStatus[account.id] = 'error'
    }
  }

  /** Persist the sender (person) name (#115). */
  async function onPersonNameChange(account: Account, raw: string) {
    const next = raw.trim() || null
    account.person_name = next
    try {
      await invoke('update_account', {
        account: { ...account, person_name: next },
      })
    } catch (e) {
      console.warn('failed to save sender name', e)
    }
  }

  /** Swap places with the next/previous account in the
   *  display-order ranking.  Re-numbers the affected pair so
   *  ties don't accumulate over time, and updates the local
   *  state immediately so the UI reorders without waiting on
   *  the round-trip. */
  async function moveAccount(account: Account, delta: -1 | 1) {
    const sorted = [...accounts].sort((a, b) => {
      const ao = a.sort_order ?? 0
      const bo = b.sort_order ?? 0
      if (ao !== bo) return ao - bo
      return a.id.localeCompare(b.id)
    })
    const idx = sorted.findIndex((a) => a.id === account.id)
    const target = idx + delta
    if (target < 0 || target >= sorted.length) return
    const other = sorted[target]
    // Re-rank from 0 so ties accumulated from older saves get
    // cleaned up.  Cheap — typical user has 1-3 accounts.
    sorted[idx] = other
    sorted[target] = account
    for (let i = 0; i < sorted.length; i++) {
      const a = sorted[i]
      if ((a.sort_order ?? 0) !== i) {
        a.sort_order = i
        try {
          await invoke('update_account', { account: { ...a, sort_order: i } })
        } catch (e) {
          console.warn('failed to save account sort order', e)
        }
      }
    }
    accounts = [...sorted]
  }

  async function persistIcons(account: Account, rules: FolderIconRule[]) {
    iconSaveStatus[account.id] = 'saving'
    account.folder_icons = rules
    try {
      await invoke('update_account', {
        account: { ...account, folder_icons: rules },
      })
      iconSaveStatus[account.id] = 'saved'
      setTimeout(() => {
        if (iconSaveStatus[account.id] === 'saved') iconSaveStatus[account.id] = ''
      }, 1500)
    } catch (e) {
      console.warn('failed to save folder icons', e)
      iconSaveStatus[account.id] = 'error'
    }
  }

  function addIconRule(account: Account) {
    const draft = iconDrafts[account.id]
    if (!draft) return
    const keyword = draft.keyword.trim()
    const icon = draft.icon.trim()
    if (!keyword || !icon) return
    const rules = [...(account.folder_icons ?? []), { keyword, icon }]
    iconDrafts[account.id] = { keyword: '', icon: '' }
    void persistIcons(account, rules)
  }

  function removeIconRule(account: Account, idx: number) {
    const rules = (account.folder_icons ?? []).filter((_, i) => i !== idx)
    void persistIcons(account, rules)
  }
</script>

<div class="h-full flex flex-col bg-surface-50 dark:bg-surface-900">
  <!-- Header bar -->
  <div class="flex items-center justify-between p-4 border-b border-surface-200 dark:border-surface-700">
    <h1 class="text-xl font-bold">Account Settings</h1>
    <button class="btn btn-sm preset-outlined-surface-500" onclick={onclose}>
      Back to Inbox
    </button>
  </div>

  <!-- Content — split into a category nav (left) + active panel
       (right) per #131.  The nav is sticky to the top of the
       scroll container so it stays visible as the right pane
       scrolls inside its own column. -->
  <div class="flex-1 overflow-hidden flex">
    <!-- Category nav.  Single source-of-truth list, keeps order
         identical between mobile-collapse + desktop layouts when
         that comes. -->
    <nav class="w-48 shrink-0 border-r border-surface-200 dark:border-surface-700 bg-surface-100/60 dark:bg-surface-800/40 overflow-y-auto p-3">
      <ul class="space-y-1">
        {#each CATEGORIES as cat (cat.id)}
          {@const active = activeCategory === cat.id}
          <li>
            <button
              type="button"
              class="w-full text-left px-3 py-2 rounded-md text-sm flex items-center gap-2 transition-colors {active
                ? 'bg-primary-500/15 text-primary-600 dark:text-primary-300 font-medium'
                : 'hover:bg-surface-200 dark:hover:bg-surface-700 text-surface-700 dark:text-surface-200'}"
              onclick={() => (activeCategory = cat.id)}
              aria-current={active ? 'page' : undefined}
            >
              <Icon name={cat.icon} size={16} />
              <span>{cat.label}</span>
            </button>
          </li>
        {/each}
      </ul>
    </nav>

    <!-- Active category's panel — scrolls independently of the
         nav so long sections (the account list) don't push the
         nav off-screen. -->
    <div class="flex-1 overflow-y-auto p-6 max-w-3xl w-full">
    {#if activeCategory === 'general'}
    <!-- App Preferences (Issue #16) — always visible, independent
         of per-account loading state so the user can tweak tray /
         sync / notification behaviour even before adding an account. -->
    <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg mb-6">
      <div class="flex items-center justify-between mb-3">
        <h2 class="text-base font-semibold">General</h2>
        <div class="flex items-center gap-2">
          {#if prefsSaveStatus === 'saving'}
            <span class="text-xs text-surface-400">Saving…</span>
          {:else if prefsSaveStatus === 'saved'}
            <span class="text-xs text-success-500">Saved</span>
          {:else if prefsSaveStatus === 'error'}
            <span class="text-xs text-error-500">Save failed</span>
          {/if}
        </div>
      </div>

      <div class="space-y-3 text-sm">
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.minimize_to_tray}
            label="Minimize to tray when closing the window"
            onchange={() => scheduleSave()}
          />
          <span>Minimize to tray when closing the window</span>
        </div>

        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.start_minimized}
            label="Start minimized to tray"
            onchange={() => scheduleSave()}
          />
          <span>Start minimized to tray</span>
        </div>

        <div class="flex items-start gap-3">
          <Toggle
            checked={appSettings.autostart_enabled}
            label="Launch Unkai when I sign in"
            onchange={(v) => void onAutostartToggle(v)}
          />
          <span>
            Launch Unkai when I sign in
            <span class="block text-xs text-surface-500">
              Adds an entry to your OS's autostart list. Combine with "Start minimized to tray" for a quiet boot.
            </span>
          </span>
        </div>

        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.auto_advance_after_remove}
            label="After delete / archive, open the next message automatically"
            onchange={() => scheduleSave()}
          />
          <span>After delete / archive, open the next message automatically</span>
        </div>

        <!-- #260 — toggle: when a `mail://` reference inside a
             note is clicked, ON routes through the main view-
             switch so the inbox lands on that message; OFF
             (default) opens a standalone reader window and the
             Notes view stays put. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.notes_mail_open_in_view as boolean}
            label={m.settings_notes_mail_open_in_view_label()}
            onchange={() => scheduleSave()}
          />
          <span>
            {m.settings_notes_mail_open_in_view_label()}
            <span class="block text-xs text-surface-500">
              {m.settings_notes_mail_open_in_view_hint()}
            </span>
          </span>
        </div>

        <!-- #280 — location autocomplete + inline map preview.
             Off by default because each typed query goes to
             nominatim.openstreetmap.org and the embedded map
             loads tiles from openstreetmap.org — both outside
             the user's Nextcloud trust boundary.  Spelling out
             the dependency in the description lets the user
             make an informed call. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.location_geocoding_enabled}
            label={m.settings_location_geocoding_label()}
            onchange={() => scheduleSave()}
          />
          <div class="flex-1">
            <span>{m.settings_location_geocoding_label()}</span>
            <p class="text-xs text-surface-400 mt-0.5">
              {m.settings_location_geocoding_help()}
            </p>
            <!-- #259 follow-up — Nominatim self-host override.
                 Empty = use the public default; the placeholder
                 surfaces it so the user always knows what
                 they're departing from.  Reset button clears
                 the field, which the backend treats as "fall
                 back to default". -->
            {#if appSettings.location_geocoding_enabled}
              <div class="flex items-center gap-2 mt-2">
                <label class="text-xs text-surface-500 shrink-0" for="nominatim-base-url">
                  {m.settings_nominatim_base_url_label()}
                </label>
                <input
                  id="nominatim-base-url"
                  type="url"
                  class="input flex-1 text-sm py-1 px-2 rounded-md"
                  placeholder={DEFAULT_NOMINATIM_BASE_URL}
                  bind:value={appSettings.nominatim_base_url}
                  onchange={() => scheduleSave()}
                />
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 shrink-0"
                  disabled={appSettings.nominatim_base_url.trim() === ''}
                  onclick={() => {
                    appSettings.nominatim_base_url = ''
                    scheduleSave()
                  }}
                >
                  {m.settings_nominatim_base_url_reset()}
                </button>
              </div>
              <p class="text-xs text-surface-500 mt-1">
                {m.settings_nominatim_base_url_hint()}
              </p>
            {/if}
          </div>
        </div>

        <!-- Display language (#190).  Auto by default — paraglide
             picks from `navigator.language` (the OS-reported
             language) on launch.  The dropdown forces a specific
             locale; flipping it back to "Auto" clears the pin and
             returns to OS-following.  Changes apply live via
             `setLocale` so the user sees the effect immediately. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.ui_locale_auto as boolean}
            label={m.settings_general_language_auto_label()}
            onchange={() => {
              if (appSettings.ui_locale_auto) {
                // Returning to auto — clear the pinned value so
                // paraglide's `preferredLanguage` strategy
                // (`navigator.language`) wins on the next
                // restart.
                appSettings.ui_locale = ''
                try {
                  window.localStorage.removeItem('PARAGLIDE_LOCALE')
                } catch {}
                // Show the restart prompt only when the
                // resolved auto-locale would actually differ
                // from what's running now.
                const wouldBe = (
                  navigator.language || initialLocale
                ).split('-')[0]
                if (wouldBe && wouldBe !== initialLocale) {
                  restartPromptOpen = true
                }
              } else {
                // Switching FROM auto TO manual — pre-fill
                // `ui_locale` (and the localStorage pin) with
                // the locale that's actually running, so the
                // dropdown that's about to appear reflects
                // reality.  Without this the dropdown
                // defaulted to `locales[0]` (English), which
                // matched what was selected only by accident
                // — a German user clicking "English" to
                // confirm wouldn't trigger an `onchange`
                // (browser elides "selected the already-
                // selected option") and the restart prompt
                // never fired.  Pre-filling means picking the
                // same locale is a no-op (correct) and
                // picking a different one always fires the
                // change handler.
                if (!appSettings.ui_locale) {
                  appSettings.ui_locale = initialLocale
                  try {
                    window.localStorage.setItem(
                      'PARAGLIDE_LOCALE',
                      initialLocale,
                    )
                  } catch {}
                }
              }
              scheduleSave()
            }}
          />
          <span>
            {m.settings_general_language_auto_label()}
            <span class="block text-xs text-surface-500">
              {m.settings_general_language_auto_hint()}
            </span>
          </span>
        </div>
        {#if !(appSettings.ui_locale_auto ?? true)}
          <label class="flex items-center gap-2 pt-1 pl-9 text-sm">
            <span class="shrink-0">{m.settings_general_language_label()}</span>
            <select
              class="select px-2 py-1 text-sm rounded-md flex-1 max-w-65"
              value={appSettings.ui_locale || (locales as readonly string[])[0]}
              onchange={(e) => {
                const v = (e.currentTarget as HTMLSelectElement).value
                appSettings.ui_locale = v
                if ((locales as readonly string[]).includes(v)) {
                  // Pin the localStorage value paraglide reads
                  // at boot.  `setLocale` is the canonical way
                  // but it triggers a reload by default and a
                  // `{#key}` remount otherwise — both had
                  // visible side-effects in earlier attempts;
                  // writing the key directly avoids them.
                  try {
                    window.localStorage.setItem('PARAGLIDE_LOCALE', v)
                  } catch {}
                }
                scheduleSave()
                if (v !== initialLocale) restartPromptOpen = true
              }}
            >
              {#each locales as code}
                <option value={code}>{LOCALE_LABELS[code] ?? code}</option>
              {/each}
            </select>
          </label>
          <p class="pl-9 text-xs text-surface-500">
            {m.settings_general_language_hint()}
          </p>
        {/if}

        <!-- File associations (#254).  Unkai is registered as an
             eligible handler for .ics / .eml during install via
             the bundle.fileAssociations entries in
             tauri.conf.json — but on Windows, "eligible" doesn't
             mean "default".  The user has to flip the toggle in
             the OS settings panel themselves; we provide a
             shortcut button instead of asking them to dig
             through Settings → Apps → Default apps by hand. -->
        <div class="pt-2 border-t border-surface-300/40 dark:border-surface-700/40">
          <div class="flex items-start justify-between gap-3">
            <div class="text-sm">
              <div>{m.settings_general_file_assoc_label()}</div>
              <div class="text-xs text-surface-500 mt-0.5">
                {m.settings_general_file_assoc_hint()}
              </div>
            </div>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 shrink-0 inline-flex items-center gap-1.5"
              onclick={openDefaultAppsSettings}
            >
              <Icon name="open-link" size={14} />
              {m.settings_general_file_assoc_button()}
            </button>
          </div>
        </div>

        <!-- Welcome tour replay (#420).  The tour auto-plays once
             after first-run setup; this is the on-demand path the
             issue asks for ("replay it later from settings"). -->
        <div class="pt-2 border-t border-surface-300/40 dark:border-surface-700/40">
          <div class="flex items-start justify-between gap-3">
            <div class="text-sm">
              <div>{m.settings_general_replay_tour_label()}</div>
              <div class="text-xs text-surface-500 mt-0.5">
                {m.settings_general_replay_tour_hint()}
              </div>
            </div>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 shrink-0 inline-flex items-center gap-1.5"
              onclick={() => onreplaytour?.()}
            >
              <Icon name="help" size={14} />
              {m.settings_general_replay_tour_button()}
            </button>
          </div>
        </div>
      </div>
    </div>
    {/if}

    {#if activeCategory === 'mail'}
    <!-- Mail-specific preferences: sync cadence + new-mail
         toast.  Account list is rendered by the gated section
         further down. -->
    <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg mb-6">
      <div class="flex items-center justify-between mb-3">
        <h2 class="text-base font-semibold">Mail preferences</h2>
        <div class="flex items-center gap-2">
          {#if prefsSaveStatus === 'saving'}
            <span class="text-xs text-surface-400">Saving…</span>
          {:else if prefsSaveStatus === 'saved'}
            <span class="text-xs text-success-500">Saved</span>
          {/if}
          <button
            class="btn btn-sm preset-outlined-primary-500 inline-flex items-center gap-1.5"
            disabled={checkNowBusy}
            onclick={runCheckMailNow}
          >
            <Icon name={checkNowBusy ? 'loading' : 'sync'} size={14} />
            {checkNowBusy ? 'Checking…' : 'Check Mail Now'}
          </button>
        </div>
      </div>
      <div class="space-y-3 text-sm">
        <!-- #165 — URLhaus link checker.  When on, every link in
             a rendered email gets a green "Safe" / red "Unsafe"
             pill, and a click on an Unsafe link goes through a
             confirm modal in MailView.  When off, links open
             without interception and the pills are suppressed. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.link_check_enabled}
            label="Check links against URLhaus"
            onchange={() => scheduleSave()}
          />
          <div class="flex-1">
            <span>Check links against URLhaus</span>
            <p class="text-xs text-surface-400 mt-0.5">
              Looks up every link in a received mail against
              <a
                href="https://urlhaus.abuse.ch/"
                target="_blank"
                rel="noopener noreferrer"
                class="text-primary-500 underline"
              >abuse.ch's URLhaus</a> malicious-URL list. Marks safe links
              with a green pill and unsafe ones with a red pill; clicking an
              unsafe link asks before opening. The list is downloaded over
              HTTPS once an hour with no per-user identifiers.
            </p>
            {#if appSettings.link_check_enabled}
              <!-- Wrapping <div> instead of <p> so the
                   `text-surface-500` on the status line stays
                   on the status line and doesn't cascade into
                   the button via `currentColor` — Skeleton's
                   outlined preset paints the label in
                   `currentColor`, so a parent `text-*` class
                   would otherwise override the theme's
                   primary-on colour and give us grey text. -->
              <div class="flex flex-wrap items-center gap-2 mt-2">
                <span class="text-xs text-surface-500">
                  Last refreshed: <strong>{formatRefreshAge(linkCheckStatus.lastRefreshedAt)}</strong>
                  · {linkCheckStatus.totalUrls.toLocaleString()} URLs in local snapshot
                </span>
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-primary-500 inline-flex items-center gap-1.5"
                  disabled={linkCheckRefreshing}
                  onclick={() => void onRefreshUrlhausNow()}
                >
                  <Icon name={linkCheckRefreshing ? 'loading' : 'sync'} size={14} />
                  {linkCheckRefreshing ? 'Refreshing…' : 'Refresh now'}
                </button>
              </div>
            {/if}
          </div>
        </div>

        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.background_sync_enabled}
            label="Run background mail sync"
            onchange={() => scheduleSave()}
          />
          <span>Run background mail sync</span>
        </div>

        <label class="flex items-center gap-2 pl-12">
          <span class="text-surface-500">Interval (seconds):</span>
          <input
            type="number"
            min="30"
            step="30"
            class="input w-24 text-sm py-1 px-2"
            disabled={!appSettings.background_sync_enabled}
            bind:value={appSettings.background_sync_interval_secs}
            onchange={scheduleSave}
          />
          <span class="text-xs text-surface-400">min. 30</span>
        </label>

        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.notifications_enabled}
            label="Show desktop notifications for new mail"
            onchange={() => scheduleSave()}
          />
          <span>Show desktop notifications for new mail</span>
        </div>

        <!-- #334 — group replies into a single conversation row in
             the MailList.  When off, every envelope renders as its
             own row (the pre-#277 flat-list behaviour).  The cache
             still tracks thread_id either way so flipping back on
             is an immediate re-render with no IMAP traffic. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.conversation_view_enabled}
            label={m.settings_mail_conversation_view_label()}
            onchange={() => scheduleSave()}
          />
          <div>
            <span>{m.settings_mail_conversation_view_label()}</span>
            <p class="text-xs text-surface-400 mt-0.5">
              {m.settings_mail_conversation_view_hint()}
            </p>
          </div>
        </div>

        <!-- #416 — read-receipt response policy (RFC 8098).  A
             receipt tells the sender when you displayed their
             mail, so it sits with the other privacy controls
             (remote images below).  `ask` keeps the per-message
             banner; `always` answers silently; `never` suppresses
             the whole flow. -->
        <label class="flex items-center gap-2">
          <span class="shrink-0">{m.settings_mail_mdn_label()}</span>
          <select
            class="select px-2 py-1 text-sm rounded-md max-w-65"
            bind:value={appSettings.mdn_response_mode}
            onchange={() => scheduleSave()}
          >
            <option value="ask">{m.settings_mail_mdn_ask()}</option>
            <option value="always">{m.settings_mail_mdn_always()}</option>
            <option value="never">{m.settings_mail_mdn_never()}</option>
          </select>
        </label>
        <p class="text-xs text-surface-400 -mt-1">
          {m.settings_mail_mdn_hint()}
        </p>

        <!-- Auto-load remote images (#197).  Off by default —
             every loaded remote image is a tracking signal back
             to the sender ("yes, this address is alive and the
             user read it at $time").  When on, the per-message
             "Remote images are blocked" banner is hidden and
             every HTML mail renders with its remote images
             loaded. Per-message "Show images" and per-sender
             trust still work the same way regardless.  Sits
             directly above the trusted-senders list so the two
             remote-image controls read as a pair. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.auto_load_remote_images}
            label="Auto-load remote images in HTML mail"
            onchange={() => scheduleSave()}
          />
          <div>
            <span>Auto-load remote images in HTML mail</span>
            <p class="text-xs text-surface-400 mt-0.5">
              Off (default) blocks remote images and shows a "Show images" banner per message — protects against tracking pixels.
              On loads every image automatically and hides the banner.
            </p>
          </div>
        </div>

        <!-- Trusted senders (#295).  Per-sender allow-list driven
             by the "Always show from X" button on the in-message
             blocked-images banner.  Listing the entries here lets
             users review what they've trusted and revoke individual
             addresses without having to reset the whole list. -->
        <div class="pt-2">
          <h3 class="text-sm font-medium mb-1">Trusted senders</h3>
          <p class="text-xs text-surface-400 mb-2">
            Addresses you've chosen "Always show from" for.  Remote images
            from these senders load automatically even when the auto-load
            toggle is off.
          </p>
          {#if trustedSenders.length === 0}
            <p class="text-xs text-surface-500 italic">
              No trusted senders yet.  Open a message with blocked images
              and use "Always show from [sender]" on the banner to add one.
            </p>
          {:else}
            <ul class="divide-y divide-surface-200 dark:divide-surface-700 rounded-md border border-surface-200 dark:border-surface-700 max-w-xl">
              {#each trustedSenders as addr (addr)}
                <li class="flex items-center gap-3 px-3 py-2">
                  <span class="flex-1 min-w-0 truncate text-sm font-mono">{addr}</span>
                  <button
                    class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                    title="Remove trusted sender"
                    aria-label="Remove trusted sender {addr}"
                    onclick={() => onRemoveTrustedSender(addr)}
                  ><Icon name="trash" size={14} /></button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      </div>
    </div>
    {/if}

    {#if activeCategory === 'calendar'}
    <!-- Calendar preferences: default calendar + the two
         reminder toggles.  Both belong here because they're
         CalDAV / event-level concerns, not mail-app behaviour. -->
    <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg mb-6">
      <div class="flex items-center justify-between mb-3">
        <h2 class="text-base font-semibold">Calendar preferences</h2>
        {#if prefsSaveStatus === 'saving'}
          <span class="text-xs text-surface-400">Saving…</span>
        {:else if prefsSaveStatus === 'saved'}
          <span class="text-xs text-success-500">Saved</span>
        {/if}
      </div>
      <div class="space-y-3 text-sm">
        <!-- Meeting reminders — events that carry a join URL
             (Talk / Zoom / Meet / Teams / Jitsi / …).  Same
             flag the Talk-only nudges used pre-#203, just
             generalised to any HTTPS meeting URL. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.meeting_reminders_enabled}
            label="Meeting reminders"
            onchange={() => scheduleSave()}
          />
          <span>
            Meeting reminders
            <span class="block text-xs text-surface-500">
              Notify me before events that include a meeting URL
              (Talk, Zoom, Meet, Teams, Jitsi, …).  Lead time
              follows the event's own reminder.
            </span>
          </span>
        </div>

        <!-- Calendar event reminders — everything *without* a
             meeting URL.  Independent toggle so users who only
             want meeting nudges can mute this without losing
             the meeting stream, and vice-versa. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.calendar_reminders_enabled}
            label="Calendar event reminders"
            onchange={() => scheduleSave()}
          />
          <span>
            Calendar event reminders
            <span class="block text-xs text-surface-500">
              Notify me before any other calendar event that has
              a reminder set.  Lead time follows the event's own
              reminder.
            </span>
          </span>
        </div>

        <!-- Auto-open meetings (#203 follow-up): when ON, a
             meeting reminder firing at "now" (≤1 min lead, e.g.
             the snooze "At event start" preset) opens the
             meeting URL in the user's default browser straight
             away instead of surfacing the reminder popup.  Off
             by default — the popup is the consistent default
             surface; this is the express-lane shortcut for
             users who'd rather skip the click. -->
        <div class="flex items-start gap-3">
          <Toggle
            bind:checked={appSettings.auto_open_meetings}
            label="Auto-open meetings at the event start"
            onchange={() => scheduleSave()}
          />
          <span>
            Auto-open meetings at the event start
            <span class="block text-xs text-surface-500">
              When a meeting's reminder fires at the event start,
              open the meeting URL in your browser straight away
              instead of showing the reminder popup.
            </span>
          </span>
        </div>

        <!-- Default calendar.  Used by the EventEditor as the
             pre-selected calendar in create-mode, and by the
             RSVP card as the default destination for accepted
             invites.  Hidden when no Nextcloud calendars are
             cached yet. -->
        {#if calendarsForPicker.length > 0}
          <label class="flex items-center gap-2 pt-2">
            <span class="shrink-0">Default calendar</span>
            <select
              class="select px-2 py-1 text-sm rounded-md flex-1 max-w-[320px]"
              bind:value={appSettings.default_calendar_id}
              onchange={scheduleSave}
            >
              <option value={null}>(use first available)</option>
              {#each calendarsForPicker as c (c.id)}
                <option value={c.id}>{c.display_name}</option>
              {/each}
            </select>
          </label>
        {/if}
      </div>
    </div>
    {/if}

    {#if activeCategory === 'design'}
    <!-- Appearance (Issue #17) — theme + light/dark mode picker.
         Changes apply live via `onThemeChange` so the user sees the
         result before navigating away from settings. -->
    <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg mb-6">
      <div class="flex items-center justify-between mb-3">
        <h2 class="text-base font-semibold">Appearance</h2>
      </div>

      <div class="space-y-4 text-sm">
        <div>
          <p class="font-medium mb-2">Mode</p>
          <div class="flex gap-2">
            {#each ['system', 'light', 'dark'] as const as mode}
              <button
                type="button"
                class="btn btn-sm {appSettings.theme_mode === mode
                  ? 'preset-filled-primary-500'
                  : 'preset-outlined-surface-500'}"
                onclick={() => onThemeChange(appSettings.theme_name, mode)}
              >
                {mode === 'system' ? 'Follow OS' : mode === 'light' ? 'Light' : 'Dark'}
              </button>
            {/each}
          </div>
          <p class="text-xs text-surface-400 mt-1">
            "Follow OS" tracks your system light/dark preference live.
          </p>
        </div>

        <!-- UI scale (#191).  Auto by default — picks a sensible
             multiplier from the screen width on every launch.
             User can override with the slider below; the slider's
             onchange flips `ui_scale_auto` off so subsequent
             launches keep the manual choice instead of snapping
             back to the auto-derived value.  Ctrl+wheel is also
             wired in App.svelte and updates the same setting. -->
        <div>
          <p class="font-medium mb-1">UI scale</p>
          <p class="text-xs text-surface-400 mb-2">
            Resize text, icons, and the rest of the UI uniformly.
            Tip: Ctrl + scroll over the app also adjusts this.
          </p>
          <label class="flex items-center gap-3 mb-3">
            <Toggle
              bind:checked={appSettings.ui_scale_auto as boolean}
              label="Auto-scale based on screen size"
              onchange={() => {
                applyUiScale(
                  effectiveScale(
                    appSettings.ui_scale,
                    appSettings.ui_scale_auto,
                  ),
                )
                scheduleSave()
              }}
            />
            <span class="text-sm">
              Auto-scale
              <span class="block text-xs text-surface-500">
                Pick a scale automatically based on the current screen.
              </span>
            </span>
          </label>
          <label class="flex items-center gap-3 text-sm">
            <span class="shrink-0 w-16">Manual</span>
            <input
              type="range"
              class="flex-1"
              min={MIN_UI_SCALE}
              max={MAX_UI_SCALE}
              step={UI_SCALE_STEP}
              disabled={appSettings.ui_scale_auto ?? true}
              bind:value={appSettings.ui_scale as number}
              onchange={() => {
                appSettings.ui_scale_auto = false
                applyUiScale(
                  effectiveScale(
                    appSettings.ui_scale,
                    appSettings.ui_scale_auto,
                  ),
                )
                scheduleSave()
              }}
            />
            <span class="font-mono text-xs w-12 text-right">
              {Math.round((appSettings.ui_scale ?? 1) * 100)}%
            </span>
          </label>
        </div>

        <div>
          <div class="flex items-center justify-between mb-2">
            <p class="font-medium">Theme</p>
            <button
              type="button"
              class="btn btn-sm preset-outlined-primary-500 text-xs"
              disabled={importingTheme}
              onclick={() => void importCustomTheme()}
              title="Pick a Skeleton-shape CSS file from disk. The Skeleton theme generator (skeleton.dev) exports compatible files; community themes also work."
            >{importingTheme ? 'Importing…' : '+ Import theme…'}</button>
          </div>
          <div class="grid grid-cols-1 sm:grid-cols-2 gap-2">
            {#each pickerThemes as theme (theme.id)}
              {@const active = appSettings.theme_name === theme.id}
              <div class="relative">
                <button
                  type="button"
                  class="w-full text-left p-3 rounded-md border transition-colors {active
                    ? 'border-primary-500 bg-primary-500/10'
                    : 'border-surface-300 dark:border-surface-700 hover:bg-surface-200 dark:hover:bg-surface-700'}"
                  onclick={() => onThemeChange(theme.id, appSettings.theme_mode)}
                >
                  <div class="font-medium flex items-center gap-2">
                    <span>{theme.label}</span>
                    {#if theme.custom}
                      <span class="text-[10px] uppercase tracking-wider font-semibold px-1 py-px rounded bg-primary-500/20 text-primary-600 dark:text-primary-300">
                        custom
                      </span>
                    {/if}
                  </div>
                  <div class="text-xs text-surface-500 mt-0.5">{theme.description}</div>
                </button>
                {#if theme.custom}
                  <button
                    type="button"
                    class="absolute top-1 right-1 w-6 h-6 rounded text-xs text-surface-500 hover:bg-error-500/20 hover:text-error-500"
                    title="Remove custom theme"
                    aria-label={`Remove ${theme.label}`}
                    onclick={(e) => {
                      e.stopPropagation()
                      void removeCustomTheme(theme.id)
                    }}
                  >×</button>
                {/if}
              </div>
            {/each}
          </div>
          <p class="text-xs text-surface-400 mt-2">
            Imported themes aren't validated — a poorly-tuned palette can hurt readability.
            Skeleton's theme generator at <span class="font-mono">skeleton.dev</span> exports compatible files.
          </p>
        </div>

        <div>
          <p class="font-medium mb-2">HTML mail background</p>
          <div class="flex flex-wrap gap-2">
            <button
              type="button"
              class="btn btn-sm {appSettings.mail_html_white_background
                ? 'preset-filled-primary-500'
                : 'preset-outlined-surface-500'}"
              onclick={() => {
                appSettings.mail_html_white_background = true
                scheduleSave()
              }}
            >Always white</button>
            <button
              type="button"
              class="btn btn-sm {!appSettings.mail_html_white_background
                ? 'preset-filled-primary-500'
                : 'preset-outlined-surface-500'}"
              onclick={() => {
                appSettings.mail_html_white_background = false
                scheduleSave()
              }}
            >Use mail's theme</button>
          </div>
          <p class="text-xs text-surface-400 mt-1">
            HTML emails usually assume a white background — "Always white" keeps
            them readable in dark mode. "Use mail's theme" lets the email render
            against the app's background, which respects dark-mode-aware emails
            but can wash out the rest. Each open mail also has its own toggle
            to override this default.
          </p>
        </div>


        <!-- App icon picker — swaps the running tray + window
             icon (and Windows taskbar entry, which mirrors the
             window icon) to one of the bundled styles.  The
             `unkai-logo://localhost/<id>` URI scheme serves the
             embedded PNG bytes so each tile previews the actual
             icon, not just a colour swatch. -->
        <div>
          <p class="font-medium mb-1">App icon</p>
          <p class="text-xs text-surface-400 mb-2">
            Affects the tray icon, the window titlebar, and (on Windows) the
            taskbar entry. The .exe icon shown in Explorer / Finder before
            launch is fixed at build time and isn't changed by this picker.
          </p>
          <div class="grid grid-cols-3 sm:grid-cols-4 gap-2">
            {#each LOGO_STYLES as style (style.id)}
              {@const active = (appSettings.logo_style ?? 'storm') === style.id}
              <button
                type="button"
                class="flex flex-col items-center gap-1 p-2 rounded-md border transition-colors {active
                  ? 'border-primary-500 bg-primary-500/10'
                  : 'border-surface-300 dark:border-surface-700 hover:bg-surface-200 dark:hover:bg-surface-700'}"
                disabled={logoSaving}
                aria-pressed={active}
                onclick={() => void pickLogoStyle(style.id)}
                title="Use the {style.label} icon for the tray, window and taskbar"
              >
                <img
                  src={convertFileSrc(style.id, 'unkai-logo')}
                  alt={`${style.label} icon preview`}
                  class="w-12 h-12 object-contain"
                  loading="lazy"
                />
                <span class="text-xs">{style.label}</span>
                <span class="block w-8 h-1 rounded-full {style.swatchTone}"></span>
              </button>
            {/each}
          </div>
        </div>
      </div>
    </div>
    {/if}

    {#if activeCategory === 'mail'}
    {#if loading}
      <p class="text-surface-500 text-center py-8">Loading accounts...</p>

    {:else if error}
      <div class="text-sm text-red-500 p-4 bg-red-500/10 rounded-md mb-4">
        {error}
      </div>

    {:else if accounts.length === 0}
      <div class="text-center py-12">
        <p class="text-surface-500 mb-4">No accounts configured yet.</p>
        <button class="btn preset-filled-primary-500" onclick={onaddaccount}>
          Add Account
        </button>
      </div>

    {:else}
      <!-- Account list — sorted by `sort_order` (#115) so the
           order in this panel matches the IconRail. -->
      {@const sortedRows = [...accounts].sort((a, b) => {
        const ao = a.sort_order ?? 0
        const bo = b.sort_order ?? 0
        if (ao !== bo) return ao - bo
        return a.id.localeCompare(b.id)
      })}
      <div class="space-y-4">
        {#each sortedRows as account, accountIdx (account.id)}
          <!-- #314 — popout trigger rendered inside the inline
               RichTextEditor's `actionsTrailing` slot.  Lives in
               the loop body so it closes over the per-iteration
               `account` and the right account gets popped out
               when the user clicks. -->
          {#snippet signaturePopoutAction()}
            <button
              type="button"
              class="tb"
              title={m.signature_popout_button_title()}
              aria-label={m.signature_popout_button_aria()}
              disabled={sigPopoutOpen[account.id]}
              onclick={() => openSignaturePopout(account)}
            ><Icon name="full-screen" size={18} /></button>
          {/snippet}
          <div class="card p-4 bg-surface-100 dark:bg-surface-800 rounded-lg">
            <div class="flex items-start justify-between">
              <div class="flex items-start gap-3">
                <!-- Reorder handle: ▲ / ▼ swap places with the
                     neighbouring row.  The sort_order field is
                     persisted via update_account so the IconRail
                     picks up the new order on its next render. -->
                <div class="flex flex-col gap-1 mt-1">
                  <button
                    type="button"
                    class="w-5 h-5 flex items-center justify-center rounded text-surface-500 hover:bg-surface-200 dark:hover:bg-surface-700 disabled:opacity-30"
                    disabled={accountIdx === 0}
                    title="Move up"
                    aria-label="Move account up"
                    onclick={() => void moveAccount(account, -1)}
                  >▲</button>
                  <button
                    type="button"
                    class="w-5 h-5 flex items-center justify-center rounded text-surface-500 hover:bg-surface-200 dark:hover:bg-surface-700 disabled:opacity-30"
                    disabled={accountIdx === sortedRows.length - 1}
                    title="Move down"
                    aria-label="Move account down"
                    onclick={() => void moveAccount(account, 1)}
                  >▼</button>
                </div>
                <div>
                  <p class="font-semibold">{account.display_name}</p>
                  <p class="text-sm text-surface-500">{account.email}</p>
                  <div class="text-xs text-surface-400 mt-2 space-y-0.5">
                    <p>IMAP: {account.imap_host}:{account.imap_port}</p>
                    <p>SMTP: {account.smtp_host}:{account.smtp_port}</p>
                    {#if account.use_jmap}
                      <p class="text-primary-500">JMAP enabled</p>
                    {/if}
                  </div>
                </div>
              </div>
              <div class="flex items-center gap-1">
                <button
                  class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center"
                  title="Connection settings — edit server hostnames, ports, password, and trust certificates"
                  aria-label="Connection settings"
                  onclick={() => openServerEdit(account)}
                ><Icon name="settings" size={16} /></button>
                <button
                  class="btn btn-sm preset-outlined-surface-500 inline-flex items-center justify-center hover:bg-red-500/15 hover:text-red-500 hover:border-red-500/40"
                  title="Remove account"
                  aria-label="Remove account {account.email}"
                  onclick={() => removeAccount(account.id, account.email)}
                ><Icon name="trash" size={16} /></button>
              </div>
            </div>

            <!-- Identity fields: emoji avatar + person name (#115). -->
            <div class="mt-4 pt-4 border-t border-surface-200 dark:border-surface-700 grid grid-cols-[auto_1fr] gap-3 items-center">
              <label class="text-sm font-medium" for="emoji-{account.id}">Avatar emoji</label>
              <div class="relative">
                <button
                  id="emoji-{account.id}"
                  type="button"
                  class="input text-lg text-center w-16 px-2 py-1 rounded-md hover:bg-surface-200 dark:hover:bg-surface-700"
                  aria-label="Account emoji avatar"
                  onclick={(e) => {
                    e.stopPropagation()
                    emojiPickerForAccount = emojiPickerForAccount === account.id ? null : account.id
                  }}
                >{account.emoji || '📨'}</button>
                {#if emojiPickerForAccount === account.id}
                  <div
                    class="absolute left-0 top-full mt-1 z-50"
                    role="menu"
                    tabindex="-1"
                    onclick={(e) => e.stopPropagation()}
                    onmousedown={(e) => e.stopPropagation()}
                    onkeydown={(e) => { if (e.key === 'Escape') emojiPickerForAccount = null }}
                  >
                    <EmojiPicker
                      value={account.emoji ?? null}
                      onpick={(emoji) => {
                        emojiPickerForAccount = null
                        void onEmojiChange(account, emoji)
                      }}
                    />
                  </div>
                {/if}
              </div>
              <label class="text-sm font-medium" for="person-{account.id}">Sender name</label>
              <input
                id="person-{account.id}"
                type="text"
                placeholder={account.display_name}
                value={account.person_name ?? ''}
                onchange={(e) => void onPersonNameChange(account, (e.currentTarget as HTMLInputElement).value)}
                class="input flex-1 text-sm px-3 py-1 rounded-md"
                aria-label="Sender display name"
              />
            </div>
            <p class="text-xs text-surface-400 mt-1 ml-1">
              The sender name appears as <code>"Name" &lt;email&gt;</code> on outgoing mail. Defaults to the account name when empty.
            </p>

            <div class="mt-4 pt-4 border-t border-surface-200 dark:border-surface-700">
              <div class="flex items-center justify-between mb-1">
                <label class="text-sm font-medium" for="sig-{account.id}">Signature</label>
                {#if sigSaveStatus[account.id] === 'saving'}
                  <span class="text-xs text-surface-400">{m.signature_status_saving()}</span>
                {:else if sigSaveStatus[account.id] === 'saved'}
                  <span class="text-xs text-success-500">{m.signature_status_saved()}</span>
                {:else if sigSaveStatus[account.id] === 'error'}
                  <span class="text-xs text-error-500">{m.signature_status_error()}</span>
                {/if}
              </div>
              <!-- #248 — rich-text signature.  Same Tiptap editor
                   the Compose flow uses, just instantiated in a
                   fixed-height container so it sits cleanly inside
                   the settings panel.  The user can author with
                   bold / italic / lists / images / colours; the
                   HTML output rides through `update_account` →
                   `Account.signature` and Compose's `signatureBlock`
                   passes it through verbatim with the RFC 3676
                   `-- ` separator above.
                   #314 — when the popout window is open for this
                   account we lock the inline editor instead of
                   keeping two live editors over the same field. -->
              {#if sigPopoutOpen[account.id]}
                <div
                  class="signature-editor-shell flex items-center justify-center text-center px-6"
                >
                  <span class="text-sm text-surface-500">
                    {m.signature_popout_locked_banner()}
                  </span>
                </div>
              {:else}
                <div class="signature-editor-shell">
                  <RichTextEditor
                    content={account.signature ?? ''}
                    placeholder="Appended to new messages sent from this account."
                    onchange={(html) => onSignatureChange(account, html)}
                    actionsTrailing={signaturePopoutAction}
                    actionsTrailingCompact={true}
                  />
                </div>
              {/if}
            </div>

            <!-- Folder icon rules (Issue #63). Match a folder name
                 against a keyword and show the chosen icon next to
                 it in the sidebar. Useful for personal categories
                 ("Bank", "Amazon", a project name) where the IMAP
                 special-use attributes don't help. -->
            <div class="mt-4 pt-4 border-t border-surface-200 dark:border-surface-700">
              <div class="flex items-center justify-between mb-2">
                <span class="text-sm font-medium">Folder icons</span>
                {#if iconSaveStatus[account.id] === 'saving'}
                  <span class="text-xs text-surface-400">Saving…</span>
                {:else if iconSaveStatus[account.id] === 'saved'}
                  <span class="text-xs text-success-500">Saved</span>
                {:else if iconSaveStatus[account.id] === 'error'}
                  <span class="text-xs text-error-500">Save failed</span>
                {/if}
              </div>

              {#if (account.folder_icons ?? []).length > 0}
                <ul class="space-y-1 mb-2">
                  {#each account.folder_icons ?? [] as rule, i (`${rule.keyword}:${i}`)}
                    <li class="flex items-center gap-2 text-sm">
                      <span class="text-lg w-6 text-center">{rule.icon}</span>
                      <span class="text-surface-500">contains</span>
                      <span class="font-mono">{rule.keyword}</span>
                      <button
                        type="button"
                        class="ml-auto text-xs text-error-500 hover:underline"
                        onclick={() => removeIconRule(account, i)}
                      >Remove</button>
                    </li>
                  {/each}
                </ul>
              {/if}

              {#if iconDrafts[account.id]}
                <div class="flex gap-2 items-center">
                  <div class="relative">
                    <button
                      type="button"
                      class="input text-lg text-center w-14 px-2 py-1 rounded-md hover:bg-surface-200 dark:hover:bg-surface-700"
                      aria-label="Icon"
                      onclick={(e) => {
                        e.stopPropagation()
                        iconDraftPickerFor = iconDraftPickerFor === account.id ? null : account.id
                      }}
                    >{iconDrafts[account.id].icon || '🏦'}</button>
                    {#if iconDraftPickerFor === account.id}
                      <div
                        class="absolute left-0 top-full mt-1 z-50"
                        role="menu"
                        tabindex="-1"
                        onclick={(e) => e.stopPropagation()}
                        onmousedown={(e) => e.stopPropagation()}
                        onkeydown={(e) => { if (e.key === 'Escape') iconDraftPickerFor = null }}
                      >
                        <EmojiPicker
                          value={iconDrafts[account.id].icon || null}
                          allowClear={false}
                          onpick={(emoji) => {
                            iconDrafts[account.id].icon = emoji ?? ''
                            iconDraftPickerFor = null
                          }}
                        />
                      </div>
                    {/if}
                  </div>
                  <input
                    type="text"
                    placeholder="bank"
                    bind:value={iconDrafts[account.id].keyword}
                    onkeydown={(e) => e.key === 'Enter' && addIconRule(account)}
                    class="input flex-1 text-sm px-3 py-1 rounded-md"
                    aria-label="Folder name keyword"
                  />
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-primary-500"
                    disabled={!iconDrafts[account.id].icon.trim() || !iconDrafts[account.id].keyword.trim()}
                    onclick={() => addIconRule(account)}
                  >Add</button>
                </div>
              {/if}
              <p class="text-xs text-surface-400 mt-1">
                Match is case-insensitive against any folder whose
                name contains the keyword.
              </p>
            </div>

            <!-- End-to-end mail encryption (#57). -->
            <EncryptionSettings
              account={{ id: account.id, email: account.email }}
              oncrypto_changed={() => { void loadAccounts() }}
            />

            <!-- S/MIME (X.509) mail encryption (#338). -->
            <SmimeSettings
              account={{ id: account.id, email: account.email }}
              oncrypto_changed={() => { void loadAccounts() }}
            />
          </div>
        {/each}
      </div>

      <!-- Add another account -->
      <div class="mt-6">
        <button class="btn preset-outlined-primary-500" onclick={onaddaccount}>
          + Add Another Account
        </button>
      </div>
    {/if}
    {/if}

    {#if activeCategory === 'nextcloud'}
    <NextcloudSettings {onnextcloudchanged} />
    {/if}

    {#if activeCategory === 'security'}
    <SecuritySettings />
    {/if}

    {#if activeCategory === 'backup'}
      <!-- Issue #168: Backup & Sync.
           Two surfaces in one panel:
             1. Local — Download / Upload settings.json via the OS
                file dialog.  Cross-machine portable; users can
                stash a copy in their own backup pipeline.
             2. Nextcloud — pick a connected NC, the worker keeps
                /Unkai Mail/settings/settings.json fresh.  Used
                for first-launch recovery on a new install. -->
      <section class="space-y-6">
        <header>
          <h2 class="text-xl font-semibold">Backup &amp; Sync</h2>
          <p class="text-sm text-surface-500 mt-1 max-w-2xl">
            Save every preference, account metadata, folder→emoji
            mapping, signature, theme, and locale to a portable
            <code>settings.json</code>. Passwords stay in your
            OS keychain — they're <em>not</em> in the bundle, so
            restoring on a fresh install will still ask you to
            re-authenticate each account on first connect.
          </p>
        </header>

        <!-- Local backup -->
        <div class="rounded-md border border-surface-200 dark:border-surface-700 p-4 space-y-3">
          <div>
            <h3 class="font-medium leading-tight">Local backup</h3>
            <p class="text-xs text-surface-500 leading-snug mt-1">
              Save the bundle to a path of your choice. Re-import
              it on this or another machine.
            </p>
          </div>
          <div class="flex flex-wrap gap-2">
            <button
              class="btn preset-filled-primary-500"
              disabled={backupStatus.kind === 'busy'}
              onclick={() => void onDownloadClick()}
            >Download settings.json</button>
            <button
              class="btn preset-outlined-surface-500"
              disabled={backupStatus.kind === 'busy'}
              onclick={() => void onUploadClick()}
            >Import from file…</button>
          </div>
        </div>

        <!-- Nextcloud recovery sync -->
        <div class="rounded-md border border-surface-200 dark:border-surface-700 p-4 space-y-3">
          <div>
            <h3 class="font-medium leading-tight">Save to Nextcloud</h3>
            <p class="text-xs text-surface-500 leading-snug mt-1">
              Keep a copy on a connected Nextcloud at
              <code>/Unkai Mail/settings/settings.json</code>.
              Pushed automatically on every settings change. If
              the server is unreachable the push is queued and
              retried — silently — the next time it's online or
              the next time a setting changes.
              {#if syncState.pending}
                <br>
                <span class="text-warning-600 dark:text-warning-400">
                  A previous push is still pending — will retry shortly.
                </span>
              {/if}
            </p>
          </div>
          {#if ncOptions.length === 0}
            <p class="text-xs text-surface-500 italic">
              Connect a Nextcloud first under <strong>Nextcloud</strong> to enable this.
            </p>
          {:else}
            <div class="flex flex-wrap items-center gap-3">
              <label class="text-sm text-surface-700 dark:text-surface-300" for="sync-target">
                Sync target
              </label>
              <select
                id="sync-target"
                class="select w-72 max-w-full text-sm px-3 py-1.5 rounded-md"
                value={syncState.targetNcId ?? ''}
                onchange={(e) => {
                  const v = (e.currentTarget as HTMLSelectElement).value
                  void onSyncTargetChange(v ? v : null)
                }}
              >
                <option value="">— Off —</option>
                {#each ncOptions as opt (opt.id)}
                  <option value={opt.id}>{opt.label}</option>
                {/each}
              </select>
              {#if syncState.targetNcId}
                <button
                  class="btn btn-sm preset-outlined-surface-500"
                  disabled={backupStatus.kind === 'busy'}
                  onclick={() => void onRestoreFromNcClick()}
                >Restore from Nextcloud</button>
              {/if}
            </div>
          {/if}
        </div>

        {#if backupStatus.kind !== 'idle'}
          <p
            class="text-sm wrap-break-word {backupStatus.kind === 'error'
              ? 'text-red-500'
              : backupStatus.kind === 'done'
              ? 'text-success-600 dark:text-success-400'
              : 'text-surface-500'}"
          >{backupStatus.msg}</p>
        {/if}
      </section>
    {/if}
    </div>
  </div>
</div>

<!-- TLS re-trust confirm. Shown after `startRetrust` probes the
     IMAP host and captures a leaf cert. The user sees the SHA-256
     so they can compare against what they expected (matches the
     fingerprint Nextcloud / Let's Encrypt / their CA prints) before
     trusting it. -->
{#if serverEditAccount}
  {@const acc = serverEditAccount}
  {@const draft = serverDrafts[acc.id]}
  {@const sStatus = serverSaveStatus[acc.id]}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget) closeServerEdit() }}
    onkeydown={(e) => { if (e.key === 'Escape') closeServerEdit() }}
  >
    <div class="bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl w-lg max-w-full mx-4 p-6 space-y-5">
      <div class="flex items-start justify-between gap-3">
        <div>
          <h3 class="text-base font-semibold">Connection settings</h3>
          <p class="text-xs text-surface-500 mt-0.5">{acc.email}</p>
        </div>
        <div class="flex items-center gap-3">
          {#if sStatus === 'saving'}
            <span class="text-xs text-surface-400">Saving…</span>
          {:else if sStatus === 'saved'}
            <span class="text-xs text-success-500">Saved</span>
          {:else if sStatus === 'error'}
            <span class="text-xs text-error-500">Save failed</span>
          {/if}
          <button
            type="button"
            class="btn btn-sm preset-outlined-surface-500 inline-flex items-center gap-1.5"
            disabled={trustBusy}
            title="Probe the IMAP server's current TLS certificate and add it to this account's trust list. Use after a server cert renewal if connections start failing with 'invalid peer certificate / UnknownIssuer'."
            onclick={() => void startRetrust(acc)}
          ><Icon name="lock" size={16} /> {trustBusy ? '…' : 'Trust server cert'}</button>
        </div>
      </div>

      <div class="grid grid-cols-[1fr_6rem] gap-3">
        <label class="block">
          <span class="text-xs text-surface-500">IMAP host</span>
          <input
            type="text"
            bind:value={draft.imap_host}
            class="input w-full text-sm px-3 py-2 rounded-md mt-1"
          />
        </label>
        <label class="block">
          <span class="text-xs text-surface-500">Port</span>
          <input
            type="number"
            bind:value={draft.imap_port}
            class="input w-full text-sm px-3 py-2 rounded-md mt-1"
          />
        </label>
        <label class="block">
          <span class="text-xs text-surface-500">SMTP host</span>
          <input
            type="text"
            bind:value={draft.smtp_host}
            class="input w-full text-sm px-3 py-2 rounded-md mt-1"
          />
        </label>
        <label class="block">
          <span class="text-xs text-surface-500">Port</span>
          <input
            type="number"
            bind:value={draft.smtp_port}
            class="input w-full text-sm px-3 py-2 rounded-md mt-1"
          />
        </label>
        <label class="block col-span-2">
          <span class="text-xs text-surface-500">Password</span>
          <input
            type="password"
            autocomplete="new-password"
            placeholder="Leave empty to keep current password"
            bind:value={passwordDrafts[acc.id]}
            class="input w-full text-sm px-3 py-2 rounded-md mt-1"
          />
          <span class="block text-xs text-surface-400 mt-1">
            When set, replaces the password stored in your OS keychain. Takes effect on the next IMAP/SMTP connection.
          </span>
        </label>
      </div>

      <div class="flex justify-end gap-2 pt-2 border-t border-surface-200 dark:border-surface-700">
        <button
          type="button"
          class="btn preset-outlined-surface-500"
          onclick={closeServerEdit}
        >Close</button>
        <button
          type="button"
          class="btn preset-filled-primary-500"
          disabled={sStatus === 'saving'}
          onclick={() => void saveConnectionSettings(acc)}
        >Save</button>
      </div>
    </div>
  </div>
{/if}

{#if trustPrompt}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    tabindex="-1"
    onmousedown={(e) => { if (e.target === e.currentTarget && !trustBusy) cancelRetrust() }}
  >
    <div class="bg-surface-50 dark:bg-surface-900 rounded-lg shadow-xl w-md max-w-full p-5">
      <h3 class="text-base font-semibold mb-1">Trust this server certificate?</h3>
      <p class="text-xs text-surface-500 mb-3">
        For <span class="font-medium text-surface-700 dark:text-surface-300">{trustPrompt.account.email}</span>
        on <span class="font-mono">{trustPrompt.account.imap_host}:{trustPrompt.account.imap_port}</span>.
        Compare the SHA-256 against what your server admin (or Nextcloud's
        <em>Personal → Security</em> page) shows before clicking Trust.
      </p>

      <div class="text-xs text-surface-500 mb-1">
        SHA-256 fingerprint{trustPrompt.chain.length === 1 ? '' : 's'}
        ({trustPrompt.chain.length === 1
          ? 'leaf'
          : `leaf + ${trustPrompt.chain.length - 1} intermediate${trustPrompt.chain.length === 2 ? '' : 's'}`})
      </div>
      <ul class="font-mono text-xs wrap-break-word p-2 rounded bg-surface-100 dark:bg-surface-800 mb-3 space-y-1">
        {#each trustPrompt.chain as entry, i (entry.sha256)}
          <li>
            <span class="text-surface-500">{i === 0 ? 'leaf:' : `int${i}:`}</span>
            {entry.sha256}
          </li>
        {/each}
      </ul>

      {#if trustError}
        <p class="text-xs text-red-500 mb-3 wrap-break-word">{trustError}</p>
      {/if}

      <div class="flex justify-end gap-2">
        <button
          class="btn preset-outlined-surface-500"
          disabled={trustBusy}
          onclick={cancelRetrust}
        >Cancel</button>
        <button
          class="btn preset-filled-primary-500"
          disabled={trustBusy}
          onclick={() => void commitRetrust()}
        >{trustBusy ? 'Trusting…' : 'Trust'}</button>
      </div>
    </div>
  </div>
{/if}

<!-- Standalone error surface for the probe path — fires when
     `startRetrust` itself errored (no modal opens) so the user
     still sees what went wrong. -->
{#if trustError && !trustPrompt}
  <div class="fixed bottom-4 right-4 z-50 max-w-sm bg-red-500/95 text-white text-sm rounded-md shadow-lg px-3 py-2">
    {trustError}
    <button
      class="ml-2 underline"
      onclick={() => (trustError = '')}
    >Dismiss</button>
  </div>
{/if}

<!-- Restart-required prompt for language changes (#190).
     Fires from the locale dropdown / auto-toggle when the
     pick differs from what's currently running.  "Restart
     later" just dismisses the modal — the new pin is already
     persisted, so the next manual launch picks it up.
     "Restart now" calls the Tauri `restart_app` IPC which
     re-execs the binary; settings have been saved by then via
     `scheduleSave`. -->
{#if restartPromptOpen}
  <div
    class="fixed inset-0 z-60 flex items-center justify-center bg-black/50"
    role="dialog"
    aria-modal="true"
    aria-labelledby="restart-required-title"
  >
    <div class="card p-5 max-w-sm w-[90%] bg-surface-100 dark:bg-surface-800 rounded-lg shadow-xl">
      <h2 id="restart-required-title" class="text-base font-semibold mb-2">
        {m.settings_general_language_restart_title()}
      </h2>
      <p class="text-sm text-surface-600 dark:text-surface-300 mb-4">
        {m.settings_general_language_restart_body()}
      </p>
      <div class="flex justify-end gap-2">
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500"
          onclick={() => (restartPromptOpen = false)}
        >
          {m.settings_general_language_restart_later()}
        </button>
        <button
          type="button"
          class="btn btn-sm preset-filled-primary-500"
          onclick={() => {
            // Fire-and-forget — `restart_app` re-execs the
            // binary; once it completes the current webview is
            // already gone, so awaiting is moot.  The
            // `scheduleSave` debounce (~600 ms) might still be
            // pending, so persist explicitly here so the new
            // process boots with the right locale even on a
            // slow disk.
            void invoke('set_app_settings', {
              settings: appSettings,
            }).finally(() => {
              void invoke('restart_app')
            })
          }}
        >
          {m.settings_general_language_restart_now()}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* #248 — fixed-height shell for the signature `RichTextEditor`.
     Tiptap's editor expects a parent with bounded height (its
     scroller is `flex-1 min-h-0`); without one the toolbar would
     stack on top of an unbounded body. */
  :global(.signature-editor-shell) {
    height: 18rem;
    min-height: 18rem;
    border: 1px solid var(--color-surface-300);
    border-radius: 0.375rem;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  :global([data-mode='dark'] .signature-editor-shell) {
    border-color: var(--color-surface-700);
  }
</style>
