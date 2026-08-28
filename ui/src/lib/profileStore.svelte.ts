/**
 * profileStore — single source of truth for the profile list, the
 * window's own profile, and the startup mode (#534, modeled on
 * `contactsStore.svelte.ts`).
 *
 * Profiles are machine-global state (one `profiles.json` per
 * install), so unlike the per-profile stores this one also listens
 * for the backend's `profiles-changed` broadcast: any mutation —
 * from this window or, come chunk 4 (#535), another profile's
 * window — re-reads the registry so every consumer repaints.
 * Components subscribe to the store; nothing re-fetches privately.
 */

import * as api from './api'
import type { Profile, StartupMode } from './api'

class ProfileStore {
  /** Every profile, in registry (creation) order. */
  profiles = $state<Profile[]>([])

  /** The profile THIS window belongs to. Deleting it is refused by
   *  the backend, so the management UI disables the affordance. */
  currentId = $state<string | null>(null)

  startupMode = $state<StartupMode>({ mode: 'last_used' })

  /** Map for O(1) row lookups (rail switcher in chunk 4). */
  byId = $derived.by(() => {
    const m = new Map<string, Profile>()
    for (const p of this.profiles) m.set(p.id, p)
    return m
  })

  #initialized = false

  /** Re-read everything from the registry. Failures stay silent
   *  beyond a console warning — an unreadable registry leaves the
   *  previous snapshot standing rather than blanking the UI. */
  async load(): Promise<void> {
    try {
      const [profiles, currentId, startupMode] = await Promise.all([
        api.profiles.listProfiles(),
        api.profiles.getCurrentProfile(),
        api.profiles.getStartupMode(),
      ])
      this.profiles = profiles
      this.currentId = currentId
      this.startupMode = startupMode
    } catch (e) {
      console.warn('profileStore.load failed', e)
    }
  }

  /** First load + the profiles-changed subscription. Called once
   *  from App.svelte's mount; safe to call again (no double
   *  subscription). The listener lives as long as the window — the
   *  registry outlives any one view, so there is nothing to tear
   *  down before the window itself goes away. */
  async init(): Promise<void> {
    if (this.#initialized) return
    this.#initialized = true
    void api.onAppEvent('profiles-changed', () => void this.load())
    await this.load()
  }
}

export const profileStore = new ProfileStore()
