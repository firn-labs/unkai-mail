// Settings backup & sync helpers (#168).
//
// Three roles:
//   1. Pack & download — collect every localStorage pref Unkai
//      cares about and ask the Rust side to splice it together
//      with `AppSettings` + accounts into a single JSON blob,
//      then save it via the OS file dialog.
//   2. Upload & restore — pick a JSON file from disk, hand it
//      to the Rust side which writes everything back, then mirror
//      the bundle's `localStorage` portion into the live storage.
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

import { invoke } from '@tauri-apps/api/core'
import { open as openFileDialog, save as saveFileDialog } from '@tauri-apps/plugin-dialog'

/**
 * `localStorage` keys that carry user-visible state we want to
 * back up.  Adding a new key here means it'll automatically be
 * included in the next bundle write.  Keep this list curated:
 * arbitrary keys (like one-shot dismissed-banner flags) probably
 * don't deserve to ride along between machines.
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
  // Trusted-senders allow-list for remote-image autoload.
  // Stored as a JSON array; we copy it verbatim so the import
  // path doesn't have to know its inner shape.
  'unkai-trusted-senders',
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
  } catch {
    // localStorage may be unavailable in some webview modes; the
    // bundle still works — it'll just carry an empty map.
  }
  return out
}

/**
 * Write each key from `map` back into `localStorage`.  Keys not
 * present in `map` but present in `SYNCED_LOCAL_STORAGE_KEYS` are
 * removed — restoring a bundle should mirror the source machine's
 * state, not merge with whatever was already there.
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
    await invoke('notify_settings_changed', { localStorage: collectLocalStorage() })
  } catch (e) {
    // Failing to update the worker's snapshot is not user-
    // visible — log and move on so the UI action that triggered
    // this isn't held up by a backend hiccup.
    console.warn('notify_settings_changed failed:', e)
  }
}

/** Build a bundle JSON string from the live state. */
export async function packBundle(): Promise<string> {
  return invoke<string>('build_settings_bundle', {
    localStorage: collectLocalStorage(),
  })
}

/**
 * Save the live settings bundle to a path the user picks.
 * Returns the chosen path (so the UI can show "Saved to …") or
 * `null` if the user cancelled the save dialog.
 */
export async function downloadBundle(): Promise<string | null> {
  const path = await saveFileDialog({
    title: 'Save Unkai settings backup',
    defaultPath: 'unkai-settings.json',
    filters: [{ name: 'Unkai settings', extensions: ['json'] }],
  })
  if (!path) return null
  const json = await packBundle()
  // Reuse the existing `save_bytes_to_path` command so we don't
  // have to add the filesystem plugin to `package.json` for one
  // text write.  TextEncoder gives us a Uint8Array which Tauri
  // serialises as the `Vec<u8>` the Rust side expects.
  const bytes = new TextEncoder().encode(json)
  await invoke('save_bytes_to_path', { path, data: Array.from(bytes) })
  return path
}

/**
 * Open a file picker, read the chosen JSON, hand it to the
 * Rust side, and apply the bundle's `localStorage` portion
 * locally.  Returns `null` if the user cancelled, otherwise the
 * path that was imported.  Throws if the file doesn't parse or
 * the bundle's schema version is too new.
 */
export async function uploadBundle(): Promise<string | null> {
  const picked = await openFileDialog({
    title: 'Import Unkai settings backup',
    multiple: false,
    filters: [{ name: 'Unkai settings', extensions: ['json'] }],
  })
  if (!picked || Array.isArray(picked)) return null
  const path = typeof picked === 'string' ? picked : (picked as { path: string }).path
  const json = await invoke<string>('read_text_from_path', { path })
  const localStorageMap = await invoke<Record<string, string>>('apply_settings_bundle', { json })
  applyLocalStorage(localStorageMap)
  return path
}

/** Live view of the auto-sync state for the Settings UI. */
export interface SettingsSyncStateView {
  targetNcId: string | null
  pending: boolean
}

export async function getSyncState(): Promise<SettingsSyncStateView> {
  return invoke<SettingsSyncStateView>('get_settings_sync_state')
}

/**
 * Set (or clear, with `null`) the NC account that auto-sync
 * pushes to.  Setting it kicks off an immediate push so the
 * chosen NC has a fresh copy without waiting for the next
 * settings change.
 */
export async function setSyncTarget(targetNcId: string | null): Promise<void> {
  await invoke('set_settings_sync_target', { targetNcId })
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
  return invoke<string | null>('nc_probe_settings_bundle', { ncId })
}

/**
 * Download + apply the bundle stored on a connected NC.  Same
 * post-conditions as `uploadBundle`: every preference is
 * restored; passwords still need to be re-entered on first
 * connect.  Returns the bundle's `localStorage` portion (already
 * applied locally; returned for callers that want to inspect).
 */
export async function ncRestoreBundle(ncId: string): Promise<Record<string, string>> {
  const localStorageMap = await invoke<Record<string, string>>('nc_restore_settings_bundle', { ncId })
  applyLocalStorage(localStorageMap)
  return localStorageMap
}
