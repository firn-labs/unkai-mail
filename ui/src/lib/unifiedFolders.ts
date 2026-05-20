import { m } from '../paraglide/messages'

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
export const UNIFIED_JUNK_FOLDER = '__UnifiedJunk__'
export const UNIFIED_ARCHIVE_FOLDER = '__UnifiedArchive__'
export const UNIFIED_TRASH_FOLDER = '__UnifiedTrash__'

/** Backend-side identifier for the special-use kind a unified folder
 *  is aggregating. Matches the strings the Rust `UnifiedSpecial::parse`
 *  helper accepts. */
export type UnifiedSpecialKind = 'sent' | 'drafts' | 'junk' | 'archive' | 'trash'

/** Map a sentinel folder name back to the special-use kind the
 *  backend expects, or `null` if the folder isn't a unified special
 *  sentinel. Callers branch on the non-null result to dispatch to
 *  `fetch_unified_special_envelopes` / `get_unified_special_cached_envelopes`. */
export function unifiedSpecialKind(folder: string): UnifiedSpecialKind | null {
  switch (folder) {
    case UNIFIED_SENT_FOLDER:
      return 'sent'
    case UNIFIED_DRAFTS_FOLDER:
      return 'drafts'
    case UNIFIED_JUNK_FOLDER:
      return 'junk'
    case UNIFIED_ARCHIVE_FOLDER:
      return 'archive'
    case UNIFIED_TRASH_FOLDER:
      return 'trash'
    default:
      return null
  }
}

/** Human-friendly display name for any folder string we might hold
 *  in `selectedFolder`. Sentinels and the literal `INBOX` translate
 *  via the locale catalogue so copy referencing the folder name
 *  reads consistently with the sidebar buttons in non-English UIs.
 *  Real IMAP folder names are server-provided data — passed through
 *  as the last path segment, matching the sidebar's `displayName`. */
export function displayFolderName(folder: string): string {
  switch (folder) {
    case UNIFIED_SENT_FOLDER:
      return m.folder_name_sent()
    case UNIFIED_DRAFTS_FOLDER:
      return m.folder_name_drafts()
    case UNIFIED_JUNK_FOLDER:
      return m.folder_name_junk()
    case UNIFIED_ARCHIVE_FOLDER:
      return m.folder_name_archive()
    case UNIFIED_TRASH_FOLDER:
      return m.folder_name_trash()
  }
  if (folder.toUpperCase() === 'INBOX') return m.folder_name_inbox()
  // Real IMAP folders: strip the hierarchy prefix so "INBOX/Work"
  // shows as "Work". We don't have the server's delimiter here, so
  // fall back to `/` — matches the sidebar's behaviour when a folder
  // didn't advertise one.
  const parts = folder.split('/')
  return parts[parts.length - 1] || folder
}
