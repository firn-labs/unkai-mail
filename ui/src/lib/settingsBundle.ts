// Settings backup & sync helpers (#168).
//
// Three roles:
//   1. Pack & download — collect every localStorage pref Unkai
//      cares about and hand it to the Rust side, which splices it
//      together with `AppSettings` + accounts, opens the native
//      save dialog itself, and writes the JSON blob (#477 — no
//      filesystem path crosses the IPC boundary).
//   2. Upload & restore — the Rust side opens the file picker,
//      reads the chosen JSON, and writes everything back; we then
//      mirror the bundle's `localStorage` portion into the live
//      storage.
//   3. Notify — every settings UI mutation calls
//      `notifySettingsChanged()` to ping the auto-sync worker so
//      it can debounce + push to the configured Nextcloud.
//
// Secrets are deliberately not part of the bundle: passwords stay
// in the OS keychain, the FIDO wraps stay in the keychain
// envelope, and the SQLCipher database itself isn't exported.
// Restoring on a fresh install gives the user back every
// preference but still requires re-auth on first connect for
// each account.

import * as api from './api'
import { windowProfileReady } from './profileLocalStorage'
import { TRUSTED_SENDERS_BUNDLE_KEY, trustedSendersLocalKey } from './trustedSenders'

/**
 * `localStorage` keys that carry user-visible state we want to
 * back up, stored under the same name locally and in the bundle.
 * Adding a new key here means it'll automatically be included in
 * the next bundle write.  Keep this list curated: arbitrary keys
 * (like one-shot dismissed-banner flags) probably don't deserve
 * to ride along between machines.
 *
 * Profile-scoped keys (#535) can NOT ride here verbatim — their
 * local names embed the profile id, which differs across
 * machines.  They keep a stable wire name in the bundle and are
 * mapped explicitly in `collectLocalStorage` / `applyLocalStorage`
 * below; the trusted-senders list is the first of those.
 */
const SYNCED_LOCAL_STORAGE_KEYS = [
  // FIDO unlock toggle (#164).  The wraps live in the keychain
  // envelope — only the *user intent* (am I in encrypted mode?)
  // is on the frontend, and that intent should follow the user
  // across machines.
  'unkai.keyEncryption',
  // Display-language pin (#190).  Paraglide reads this on every
  // process start; when present it overrides
  // `navigator.language`.  Survives bundle import so a user who
  // pinned `de` on machine A doesn't get English on machine B.
  'PARAGLIDE_LOCALE',
] as const

/**
 * Read every synced key from `localStorage` into a plain map.
 * Missing keys are skipped (not encoded as null/empty) so the
 * import side can tell "not set" apart from "explicitly empty".
 */
export function collectLocalStorage(): Record<string, string> {
  const out: Record<string, string> = {}
  try {
    for (const key of SYNCED_LOCAL_STORAGE_KEYS) {
      const v = localStorage.getItem(key)
      if (v !== null) out[key] = v
    }
    // Trusted senders: profile-scoped local key → stable wire
    // name (settings sync is per-profile, so the bundle carries
    // exactly this profile's list).  Copied verbatim so the
    // import path doesn't have to know its inner shape.
    const tsKey = trustedSendersLocalKey()
    if (tsKey) {
      const v = localStorage.getItem(tsKey)
      if (v !== null) out[TRUSTED_SENDERS_BUNDLE_KEY] = v
    }
  } catch {
    // localStorage may be unavailable in some webview modes; the
    // bundle still works — it'll just carry an empty map.
  }
  return out
}

/**
 * Write each key from `map` back into `localStorage`.  Keys not
 * present in `map` but expected here are removed — restoring a
 * bundle should mirror the source machine's state, not merge with
 * whatever was already there.
 */
export function applyLocalStorage(map: Record<string, string>) {
  try {
    for (const key of SYNCED_LOCAL_STORAGE_KEYS) {
      const v = map[key]
      if (v === undefined) {
        localStorage.removeItem(key)
      } else {
        localStorage.setItem(key, v)
      }
    }
    // Trusted senders: stable wire name → this profile's scoped
    // key.  Deliberately asymmetric with the plain keys above: a
    // bundle MISSING this entry is far more likely to have been
    // collected while some window's profile id hadn't resolved
    // than to mean "the user cleared their whole allow-list", and
    // deleting on restore is unrecoverable while keeping a stale
    // local list is a visible, user-fixable state.  So absent ⇒
    // leave the local value alone.
    const tsKey = trustedSendersLocalKey()
    if (tsKey) {
      const v = map[TRUSTED_SENDERS_BUNDLE_KEY]
      if (v !== undefined) {
        localStorage.setItem(tsKey, v)
      }
    }
  } catch {
    /* storage unavailable — silent */
  }
}

/**
 * Ping the auto-sync worker.  Call after any settings UI
 * mutation so the bundle on Nextcloud (if a target is set) gets
 * refreshed.  No-op when sync is off or when no NC is reachable
 * — the worker handles failure / retry on its own.  The
 * frontend does NOT await the eventual NC PUT; this returns as
 * soon as the worker's snapshot has been updated.
 */
export async function notifySettingsChanged(): Promise<void> {
  try {
    // Wait for the window's profile id so the snapshot includes
    // the profile-scoped keys — a snapshot missing them would
    // push a bundle without the trusted-senders list (#535).
    await windowProfileReady
    await api.settings.notifySettingsChanged({ localStorage: collectLocalStorage() })
  } catch (e) {
    // Failing to update the worker's snapshot is not user-
    // visible — log and move on so the UI action that triggered
    // this isn't held up by a backend hiccup.
    console.warn('notify_settings_changed failed:', e)
  }
}

/**
 * Save the live settings bundle to a path the user picks.  The
 * dialog + write live on the Rust side (#477).  Returns the chosen
 * path (so the UI can show "Saved to …") or `null` if the user
 * cancelled the save dialog.
 */
export async function downloadBundle(): Promise<string | null> {
  await windowProfileReady
  return api.settings.exportSettingsBundle({ localStorage: collectLocalStorage() })
}

/**
 * Restore a bundle from disk.  The file picker + read + apply live
 * on the Rust side (#477); we mirror the bundle's `localStorage`
 * portion locally.  Returns `null` if the user cancelled, otherwise
 * the path that was imported.  Throws if the file doesn't parse or
 * the bundle's schema version is too new.
 */
export async function uploadBundle(): Promise<string | null> {
  const imported = await api.settings.importSettingsBundle()
  if (!imported) return null
  await windowProfileReady
  applyLocalStorage(imported.localStorage)
  return imported.path
}

/** Live view of the auto-sync state for the Settings UI. */
export interface SettingsSyncStateView {
  targetNcId: string | null
  pending: boolean
}

export async function getSyncState(): Promise<SettingsSyncStateView> {
  return api.settings.getSettingsSyncState()
}

/**
 * Set (or clear, with `null`) the NC account that auto-sync
 * pushes to.  Setting it kicks off an immediate push so the
 * chosen NC has a fresh copy without waiting for the next
 * settings change.
 */
export async function setSyncTarget(targetNcId: string | null): Promise<void> {
  await api.settings.setSettingsSyncTarget({ targetNcId })
}

/**
 * Check a connected NC for an existing settings bundle.
 * Returns the bundle's `exported_at` timestamp (RFC 3339) when
 * one is found, `null` when the path doesn't exist.  Surfaces
 * server / auth errors as exceptions; callers should catch and
 * stay quiet — this is only a probe for the "found a backup,
 * restore?" prompt.
 */
export async function ncProbeBundle(ncId: string): Promise<string | null> {
  return api.settings.ncProbeSettingsBundle({ ncId })
}

/**
 * Download + apply the bundle stored on a connected NC.  Same
 * post-conditions as `uploadBundle`: every preference is
 * restored; passwords still need to be re-entered on first
 * connect.  Returns the bundle's `localStorage` portion (already
 * applied locally; returned for callers that want to inspect).
 */
export async function ncRestoreBundle(ncId: string): Promise<Record<string, string>> {
  const localStorageMap = await api.settings.ncRestoreSettingsBundle({ ncId })
  await windowProfileReady
  applyLocalStorage(localStorageMap)
  return localStorageMap
}
