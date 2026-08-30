/**
 * In-app updater (#229) — typed wrappers over the Rust-side update
 * flow in `src-tauri/src/updater.rs`.
 *
 * The three commands form a strict sequence the backend enforces:
 * check parks the found update in machine-global state, download
 * fetches its bundle (progress rides the `update-download-progress`
 * event, targeted at the invoking window), install verifies the
 * signature and restarts into the new version.  On success install
 * never resolves — the process is replaced.
 *
 * Orchestration (schedules, skip-version, badge state) lives in
 * `../updaterStore.svelte.ts`; components talk to the store, not to
 * these wrappers.
 */

import { call } from './core'

export interface UpdateCheckResult {
  available: boolean
  currentVersion: string
  version: string | null
  /** Release notes from the manifest (the GitHub Release body). */
  notes: string | null
  /** Publish date, Unix seconds. */
  date: number | null
}

/**
 * Query the channel's release manifest. `channel` is the
 * `AppSettings.update_channel` value (`"stable"` / `"beta"`); the
 * backend maps unknown values to stable.
 */
export function checkAppUpdate(args: { channel: string }): Promise<UpdateCheckResult> {
  return call('check_app_update', args)
}

export function downloadAppUpdate(): Promise<void> {
  return call('download_app_update')
}

export function installAppUpdate(): Promise<void> {
  return call('install_app_update')
}
