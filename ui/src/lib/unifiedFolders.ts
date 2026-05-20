/**
 * Sentinel folder names used to route the global "All Sent" / "All
 * Drafts" entries through the same `selectedFolder` channel as real
 * IMAP folders.
 *
 * The unified Inbox can reuse the literal `INBOX` because that name
 * is canonical across every IMAP server we'd ship against; Sent and
 * Drafts can't (English `Sent`, German `Gesendete Elemente`, Gmail
 * `[Gmail]/Sent Mail`, …), so the sentinels below tell `MailList` and
 * the backend "this isn't a real folder name — resolve each account's
 * actual Sent/Drafts via `pick_*_folder` and merge."
 *
 * Picked with leading-and-trailing double underscores so an IMAP
 * server can't return a real folder that collides. The matching
 * routes on the backend live in `fetch_unified_special_envelopes`
 * (network) and `get_unified_special_cached_envelopes` (cache).
 */
export const UNIFIED_SENT_FOLDER = '__UnifiedSent__'
export const UNIFIED_DRAFTS_FOLDER = '__UnifiedDrafts__'

/** Backend-side identifier for the special-use kind a unified folder
 *  is aggregating. Matches the strings the Rust `UnifiedSpecial::parse`
 *  helper accepts. */
export type UnifiedSpecialKind = 'sent' | 'drafts'

/** Map a sentinel folder name back to the special-use kind the
 *  backend expects, or `null` if the folder isn't a unified special
 *  sentinel. Callers branch on the non-null result to dispatch to
 *  `fetch_unified_special_envelopes` / `get_unified_special_cached_envelopes`. */
export function unifiedSpecialKind(folder: string): UnifiedSpecialKind | null {
  if (folder === UNIFIED_SENT_FOLDER) return 'sent'
  if (folder === UNIFIED_DRAFTS_FOLDER) return 'drafts'
  return null
}
