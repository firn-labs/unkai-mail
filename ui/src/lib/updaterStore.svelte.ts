/**
 * In-app updater orchestration (#229) — one shared runes store so
 * the IconRail badge and the Updates settings page always agree on
 * what the updater is doing.
 *
 * The backend (`src-tauri/src/updater.rs`) owns the mechanics
 * (manifest fetch, signature verification, install); this store
 * owns the policy:
 *
 *   - the startup + periodic background check, gated on the user's
 *     `update_auto_check` setting
 *   - optional background download (`update_auto_download`)
 *   - the skip-this-version suppression (persisted in AppSettings,
 *     so the badge stays quiet across launches — a manual check
 *     deliberately ignores it)
 *   - the once-per-version desktop notification for a background
 *     find (a manual check already has the user's eyes on the page)
 *
 * Lifecycle: `initUpdater(prefs)` runs from App.svelte once the
 * settings snapshot exists, and again after a profile switch — the
 * `wired` flag keeps the event listener and the check timer from
 * doubling up.  With several profile windows open each window runs
 * its own schedule; that's deliberate slack, not a bug — the check
 * is one cached HTTPS GET and the parked update lives in
 * machine-global backend state, so the windows can't disagree.
 */

import * as api from './api'
import { formatError } from './errors'
import { m } from '../paraglide/messages'

/** Six hours — mail clients run for days, so a startup-only check
 *  would leave long-lived sessions on stale versions. */
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000
/** Grace delay before the startup check so the update poll never
 *  competes with the first mail sync for the network. */
const STARTUP_CHECK_DELAY_MS = 15_000

/** Machine-global (bare, un-namespaced — see profileLocalStorage's
 *  convention) marker of the last version we toasted about, so a
 *  6-hourly re-check doesn't re-notify every time. */
const NOTIFIED_VERSION_KEY = 'unkai-update-notified-version'

export const updater = $state({
  /** The running app's version (`get_app_version`). */
  currentVersion: '',
  /** Result of the last check that found something; null = current
   *  (or never checked). */
  available: null as api.updates.UpdateCheckResult | null,
  checking: false,
  downloading: false,
  /** Bundle fetched + parked backend-side; "Restart now" is live. */
  downloaded: false,
  progress: null as { downloaded: number; total: number | null } | null,
  installing: false,
  /** Sticky error from the last *manual* action — background check
   *  failures stay silent (offline is normal for a laptop). */
  error: '',
  lastCheckedAt: null as number | null,
  // Mirrors of the AppSettings updater fields, kept fresh by
  // initUpdater + the settings page's saves.
  autoCheck: true,
  autoDownload: false,
  channel: 'stable',
  skippedVersion: '',
})

/** The updater slice of `AppSettings` — index signature so the
 *  save round-trip preserves every other field (AiSettings shape). */
export interface AppSettingsUpdater {
  update_auto_check: boolean
  update_auto_download: boolean
  update_channel: string
  update_skipped_version: string
  [key: string]: unknown
}

/** Should the rail show the "update available" badge?  Suppressed
 *  for a skipped version and while the check is mid-flight. */
export function updateBadgeVisible(): boolean {
  return (
    updater.available !== null &&
    updater.available.version !== null &&
    updater.available.version !== updater.skippedVersion
  )
}

let wired = false
let checkTimer: number | null = null

/** Wire the store: version lookup, progress listener, background
 *  schedule.  Re-entrant — a profile switch re-invokes it with the
 *  new profile's prefs and only the settings mirrors update. */
export function initUpdater(prefs: AppSettingsUpdater): void {
  updater.autoCheck = prefs.update_auto_check ?? true
  updater.autoDownload = prefs.update_auto_download ?? false
  updater.channel = prefs.update_channel || 'stable'
  updater.skippedVersion = prefs.update_skipped_version ?? ''

  if (wired) return
  wired = true

  void api.system.getAppVersion().then((v) => (updater.currentVersion = v))
  void api.onAppEvent('update-download-progress', (e) => {
    updater.progress = e.payload
  })

  window.setTimeout(() => {
    if (updater.autoCheck) void checkForUpdates({ manual: false })
  }, STARTUP_CHECK_DELAY_MS)
  checkTimer = window.setInterval(() => {
    if (updater.autoCheck) void checkForUpdates({ manual: false })
  }, CHECK_INTERVAL_MS)
  // The interval lives as long as the window does — no teardown
  // path needed, but keep the handle so a future opt-out can add
  // one without re-plumbing.
  void checkTimer
}

/**
 * Ask the backend to query the channel manifest.  Background runs
 * (`manual: false`) swallow errors and fire the once-per-version
 * desktop notification; manual runs surface errors on the page and
 * ignore the skip suppression (the result is shown either way —
 * the skip only mutes the *badge*).
 */
export async function checkForUpdates(opts: { manual: boolean }): Promise<void> {
  if (updater.checking || updater.downloading || updater.installing) return
  updater.checking = true
  if (opts.manual) updater.error = ''
  try {
    const result = await api.updates.checkAppUpdate({ channel: updater.channel })
    updater.currentVersion = result.currentVersion
    updater.lastCheckedAt = Date.now()
    if (result.available) {
      // A different version than the parked one invalidates any
      // finished download — the backend re-parked the new handle.
      if (updater.available?.version !== result.version) {
        updater.downloaded = false
        updater.progress = null
      }
      updater.available = result
      if (!opts.manual) {
        if (updater.autoDownload) void downloadUpdate()
        void notifyOnce(result)
      }
    } else {
      updater.available = null
      updater.downloaded = false
      updater.progress = null
    }
  } catch (e) {
    if (opts.manual) updater.error = formatError(e) || m.settings_updates_error_check()
    else console.warn('background update check failed', e)
  } finally {
    updater.checking = false
  }
}

/** Desktop toast for a background find — once per version, gated on
 *  the OS-level notification permission only (the mail-toast
 *  setting governs mail, not app lifecycle news). */
async function notifyOnce(result: api.updates.UpdateCheckResult): Promise<void> {
  const version = result.version ?? ''
  if (!version || version === updater.skippedVersion) return
  try {
    if (window.localStorage.getItem(NOTIFIED_VERSION_KEY) === version) return
    if (!(await api.platform.notificationsPermissionGranted())) return
    api.platform.showNotification({
      title: m.update_notification_title(),
      body: m.update_notification_body({ version }),
    })
    window.localStorage.setItem(NOTIFIED_VERSION_KEY, version)
  } catch (e) {
    console.warn('update notification failed', e)
  }
}

export async function downloadUpdate(): Promise<void> {
  if (updater.downloading || updater.downloaded || !updater.available) return
  updater.downloading = true
  updater.error = ''
  updater.progress = null
  try {
    await api.updates.downloadAppUpdate()
    updater.downloaded = true
  } catch (e) {
    updater.error = formatError(e) || m.settings_updates_error_download()
  } finally {
    updater.downloading = false
  }
}

/** Verify + install the parked bundle and restart.  On success the
 *  process is replaced, so `installing` never needs a reset —
 *  the catch handles the only path that returns. */
export async function installUpdate(): Promise<void> {
  if (updater.installing || !updater.downloaded) return
  updater.installing = true
  updater.error = ''
  try {
    await api.updates.installAppUpdate()
  } catch (e) {
    updater.error = formatError(e) || m.settings_updates_error_install()
    updater.installing = false
    // Whatever failed backend-side, the parked bytes are gone (a
    // failed install consumes them; #566's wiped-stash case never
    // had them) — fall back to the Download button instead of
    // stranding a dead "Restart now".  The backend re-parks the
    // handle in both cases, so the re-download works immediately.
    updater.downloaded = false
    updater.progress = null
  }
}

/** Persist one updater-prefs mutation with the whole-struct
 *  re-fetch that keeps other panels' fields intact (the
 *  `update_app_settings` contract — see AiSettings' saveMcp),
 *  then refresh the store mirrors.  Returns the fresh settings so
 *  the settings page can hand them up to AccountSettings/App. */
export async function saveUpdaterPrefs(
  mutate: (s: AppSettingsUpdater) => void,
): Promise<AppSettingsUpdater> {
  const fresh = (await api.settings.getAppSettings()) as AppSettingsUpdater
  mutate(fresh)
  await api.settings.updateAppSettings({ newSettings: fresh })
  updater.autoCheck = fresh.update_auto_check
  updater.autoDownload = fresh.update_auto_download
  updater.channel = fresh.update_channel || 'stable'
  updater.skippedVersion = fresh.update_skipped_version ?? ''
  return fresh
}
