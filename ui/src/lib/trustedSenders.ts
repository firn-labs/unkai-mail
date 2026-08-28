// Trusted-senders allow-list for the "Always show images from
// [sender]" affordance.
//
// Stored in `localStorage` as a JSON array of lower-cased bare
// email addresses — under the PROFILE-SCOPED key
// `unkai.<profile-id>.trusted-senders` since #535 (all windows
// share one localStorage origin, and which senders you trust is
// per-profile data; the pre-#535 machine-global key is adopted
// into the window's profile once).  The list still rides along
// with every settings-bundle write (manual export + Nextcloud
// auto-sync) under the STABLE wire name `unkai-trusted-senders` —
// profile ids differ across machines, so the bundle must not
// carry them; `settingsBundle.ts` maps wire name ↔ scoped key.
//
// Every mutation calls `notifySettingsChanged()` so the
// auto-sync worker picks up the change immediately — otherwise
// the list lives only in the local browser until some *other*
// settings mutation happens to push the bundle, and a Nextcloud
// restore in between would wipe the entry (#295).

import { adoptLegacyKey, profileScopedKey } from './profileLocalStorage'
import { notifySettingsChanged } from './settingsBundle'

/** Stable settings-bundle wire name (also the pre-#535 local key). */
export const TRUSTED_SENDERS_BUNDLE_KEY = 'unkai-trusted-senders'
const SCOPED_SUFFIX = 'trusted-senders'

adoptLegacyKey(TRUSTED_SENDERS_BUNDLE_KEY, SCOPED_SUFFIX)

/** This window's profile-scoped storage key, or `null` while the
 *  profile id is still resolving (callers fall back to "no one is
 *  trusted", which is the fail-safe direction for image loading). */
export function trustedSendersLocalKey(): string | null {
  return profileScopedKey(SCOPED_SUFFIX)
}

/**
 * Strip the angle-bracketed address out of an RFC 5322 `From:`
 * string and lower-case it.  `"Jane Doe" <jane@example.org>` →
 * `jane@example.org`.  When the input has no brackets we treat
 * the whole string as an address.
 */
export function getSenderAddress(from: string): string {
  const m = from.match(/<([^>]+)>/)
  return (m ? m[1] : from).trim().toLowerCase()
}

function readList(): string[] {
  const key = trustedSendersLocalKey()
  if (!key) return []
  try {
    const raw = localStorage.getItem(key)
    return raw ? (JSON.parse(raw) as string[]) : []
  } catch {
    return []
  }
}

function writeList(list: string[]): boolean {
  const key = trustedSendersLocalKey()
  if (!key) return false
  try {
    localStorage.setItem(key, JSON.stringify(list))
    return true
  } catch {
    return false
  }
}

/** All trusted addresses, sorted alphabetically for stable UI display. */
export function listTrustedSenders(): string[] {
  return readList().slice().sort()
}

/** Whether the given `From:` header's address is on the allow-list. */
export function isSenderTrusted(from: string): boolean {
  return readList().includes(getSenderAddress(from))
}

/**
 * Add a sender's address to the allow-list.  No-op when already
 * present.  After a successful write, pings the settings-sync
 * worker so the change reaches Nextcloud before a restore could
 * overwrite it.
 */
export function addTrustedSender(from: string): void {
  const addr = getSenderAddress(from)
  if (!addr) return
  const list = readList()
  if (list.includes(addr)) return
  list.push(addr)
  if (writeList(list)) {
    void notifySettingsChanged()
  } else {
    console.warn('Failed to persist trusted sender')
  }
}

/**
 * Remove an address from the allow-list.  Accepts either a bare
 * address or a full `From:` header — both are normalised through
 * `getSenderAddress`.  No-op when not present.
 */
export function removeTrustedSender(fromOrAddr: string): void {
  const addr = getSenderAddress(fromOrAddr)
  const list = readList()
  const next = list.filter((a) => a !== addr)
  if (next.length === list.length) return
  if (writeList(next)) {
    void notifySettingsChanged()
  } else {
    console.warn('Failed to update trusted senders')
  }
}
