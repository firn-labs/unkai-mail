/**
 * Profile-scoped `localStorage` keys (#535).
 *
 * Every Tauri webview window shares ONE localStorage origin —
 * that's what the popout payload handoff relies on — so any
 * profile-scoped value must carry the profile id in its key:
 * `unkai.<profile-id>.<suffix>`.  Machine-global keys
 * (`unkai.tourCompleted`, `PARAGLIDE_LOCALE`, sidebar widths, …)
 * stay un-namespaced on purpose.
 *
 * The window's profile id is resolved once per window: seeded
 * synchronously from the `?profile=` URL param when present
 * (`profile-*` windows), then confirmed by the backend registry
 * (which is authoritative — it also covers the static main window
 * and every popout).  Until the id is known, `profileScopedKey`
 * returns `null` and callers fall back to their empty state — the
 * fail-safe direction for everything stored this way.
 */

import * as api from './api'
import { parentWindowLabel, windowProfileParam } from './windowContext'

let windowProfileId: string | null = windowProfileParam

let markReady: () => void
/** Resolves after the first profile-id resolution attempt (even a
 *  failed one).  Await this before work whose correctness depends
 *  on the scoped keys being resolvable — e.g. collecting a
 *  settings bundle, where a missing key reads as "delete on
 *  restore". */
export const windowProfileReady: Promise<void> = new Promise((resolve) => {
  markReady = resolve
})

/** Pending migrations, run as soon as the profile id is known. */
const legacyAdoptions: Array<{ legacyKey: string; suffix: string }> = []

function runAdoptions(): void {
  const pid = windowProfileId
  if (!pid) return
  // Only the static main window adopts (no `profile` param, no
  // `parent` param).  On an upgraded install the pre-#535 data
  // belongs to the startup profile, and that is exactly the
  // window hosting it — a `profile-*` fan-out window or a popout
  // resolving faster must NOT win the race and walk off with
  // another profile's data (the legacy key is deleted after the
  // copy, so a wrong adoption is unrecoverable).
  if (windowProfileParam !== null || parentWindowLabel !== null) return
  for (const { legacyKey, suffix } of legacyAdoptions) {
    try {
      const legacy = localStorage.getItem(legacyKey)
      if (legacy === null) continue
      const scoped = `unkai.${pid}.${suffix}`
      if (localStorage.getItem(scoped) === null) {
        localStorage.setItem(scoped, legacy)
      }
      localStorage.removeItem(legacyKey)
    } catch {
      /* storage unavailable — skip */
    }
  }
}

/** Resolve (or re-resolve) this window's profile id from the
 *  backend.  Called at module load; a switch-in-place uses the
 *  synchronous `setWindowProfile` instead (the caller already
 *  knows the id). */
export async function refreshWindowProfile(): Promise<void> {
  try {
    windowProfileId = await api.profiles.getCurrentProfile()
    runAdoptions()
  } catch (e) {
    console.warn('could not resolve the window profile for scoped storage:', e)
  } finally {
    markReady()
  }
}

void refreshWindowProfile()

/** Re-key the scoped storage after a switch-in-place (#535) — the
 *  new id is already in hand, no IPC round-trip needed. */
export function setWindowProfile(profileId: string): void {
  windowProfileId = profileId
}

/** `unkai.<profile-id>.<suffix>`, or `null` while the profile id
 *  is still resolving. */
export function profileScopedKey(suffix: string): string | null {
  return windowProfileId ? `unkai.${windowProfileId}.${suffix}` : null
}

/**
 * Register a one-time migration of a pre-#535 machine-global key
 * into this window's profile scope.  Runs in the static main
 * window only (see `runAdoptions`), immediately when the profile
 * id is already known, else as soon as it resolves.
 */
export function adoptLegacyKey(legacyKey: string, suffix: string): void {
  legacyAdoptions.push({ legacyKey, suffix })
  runAdoptions()
}
