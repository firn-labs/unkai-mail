// Trusted-senders allow-list for the "Always show images from
// [sender]" affordance.
//
// Stored in `localStorage` under `unkai-trusted-senders` as a
// JSON array of lower-cased bare email addresses.  The key is
// listed in `settingsBundle.ts#SYNCED_LOCAL_STORAGE_KEYS`, so it
// rides along with every settings-bundle write (manual export +
// Nextcloud auto-sync) and is restored on import.
//
// Every mutation calls `notifySettingsChanged()` so the
// auto-sync worker picks up the change immediately — otherwise
// the list lives only in the local browser until some *other*
// settings mutation happens to push the bundle, and a Nextcloud
// restore in between would wipe the entry (#295).

import { notifySettingsChanged } from './settingsBundle'

const TRUSTED_SENDERS_KEY = 'unkai-trusted-senders'

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
  try {
    const raw = localStorage.getItem(TRUSTED_SENDERS_KEY)
    return raw ? (JSON.parse(raw) as string[]) : []
  } catch {
    return []
  }
}

function writeList(list: string[]): boolean {
  try {
    localStorage.setItem(TRUSTED_SENDERS_KEY, JSON.stringify(list))
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
