/**
 * Profile management (#534): CRUD on the machine-global profile
 * registry plus the startup-mode setting.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]` in `commands::profiles`. Argument
 * keys mirror the Rust parameter names (camelCased, as Tauri expects
 * on the wire).
 *
 * Every mutation makes the backend broadcast `profiles-changed`, so
 * callers should rely on `profileStore` (which listens) rather than
 * refreshing privately.
 */

import { call } from './core'
import type { Profile, ProfileIcon, StartupMode } from './types'

export function listProfiles(): Promise<Profile[]> {
  return call('list_profiles')
}

/** The id of the profile this window belongs to. */
export function getCurrentProfile(): Promise<string> {
  return call('get_current_profile')
}

export function createProfile(args: { name: string; icon: ProfileIcon }): Promise<Profile> {
  return call('create_profile', args)
}

export function updateProfile(args: {
  id: string
  name?: string | null
  icon?: ProfileIcon | null
}): Promise<Profile> {
  return call('update_profile', args)
}

/**
 * Deletes the profile AND securely wipes its local data: cache,
 * settings, keychain entries for every account that lived in it.
 * The backend refuses the last remaining profile, the caller's own
 * profile, and any profile open in another window.
 */
export function deleteProfile(args: { id: string }): Promise<void> {
  return call('delete_profile', args)
}

export function getStartupMode(): Promise<StartupMode> {
  return call('get_startup_mode')
}

export function setStartupMode(args: { mode: StartupMode }): Promise<void> {
  return call('set_startup_mode', args)
}
