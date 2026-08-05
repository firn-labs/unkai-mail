/**
 * Message and folder operations: envelope/message fetch (live + cached),
 * folder CRUD, flags and triage state, search, unread counts,
 * attachments, and link-safety checks.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  AttachmentPreviewView,
  Email,
  EmailEnvelope,
  Folder,
  InlineImagePart,
  LinkVerdict,
  SearchFilters,
  SearchHit,
  SearchScope,
  SentReceiptStatus,
  UrlhausStatus,
} from './types'

export function fetchEnvelopes(args: {
  accountId: string
  folder: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('fetch_envelopes', args)
}

export function fetchFolders(args: { accountId: string }): Promise<Folder[]> {
  return call('fetch_folders', args)
}

export function fetchMessage(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<Email> {
  return call('fetch_message', args)
}

export function fetchOlderEnvelopes(args: {
  accountId: string
  folder: string
  beforeUid: number
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('fetch_older_envelopes', args)
}

export function fetchOlderUnifiedEnvelopes(args: {
  folder: string
  beforeUidPerAccount: Record<string, unknown>
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('fetch_older_unified_envelopes', args)
}

export function fetchUnifiedEnvelopes(args: {
  folder: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('fetch_unified_envelopes', args)
}

export function fetchUnifiedSpecialEnvelopes(args: {
  special: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('fetch_unified_special_envelopes', args)
}

export function getCachedEnvelopes(args: {
  accountId: string
  folder: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('get_cached_envelopes', args)
}

export function getCachedFolders(args: { accountId: string }): Promise<Folder[]> {
  return call('get_cached_folders', args)
}

export function getCachedMessage(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<Email | null> {
  return call('get_cached_message', args)
}

export function getUnifiedCachedEnvelopes(args: {
  folder: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('get_unified_cached_envelopes', args)
}

export function getUnifiedSpecialCachedEnvelopes(args: {
  special: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('get_unified_special_cached_envelopes', args)
}

export function getEnvelopesByThread(args: {
  accountId: string
  folder: string
  threadId: string
}): Promise<EmailEnvelope[]> {
  return call('get_envelopes_by_thread', args)
}

export function createFolder(args: { accountId: string; name: string }): Promise<void> {
  return call('create_folder', args)
}

export function deleteFolder(args: { accountId: string; name: string }): Promise<void> {
  return call('delete_folder', args)
}

export function renameFolder(args: {
  accountId: string
  oldName: string
  newName: string
}): Promise<void> {
  return call('rename_folder', args)
}

export function setFolderIcon(args: {
  accountId: string
  folderName: string
  icon?: string | null
}): Promise<void> {
  return call('set_folder_icon', args)
}

export function archiveMessage(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<void> {
  return call('archive_message', args)
}

export function archiveMessages(args: {
  accountId: string
  folder: string
  uids: number[]
}): Promise<number[]> {
  return call('archive_messages', args)
}

export function deleteMessage(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<void> {
  return call('delete_message', args)
}

export function moveMessage(args: {
  accountId: string
  folder: string
  uid: number
  destFolder: string
}): Promise<void> {
  return call('move_message', args)
}

export function moveMessages(args: {
  accountId: string
  folder: string
  uids: number[]
  destFolder: string
}): Promise<number[]> {
  return call('move_messages', args)
}

export function markAsRead(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<void> {
  return call('mark_as_read', args)
}

export function setMessageFlagged(args: {
  accountId: string
  folder: string
  uid: number
  flagged: boolean
}): Promise<void> {
  return call('set_message_flagged', args)
}

export function setMessagePinned(args: {
  accountId: string
  folder: string
  uid: number
  pinned: boolean
}): Promise<void> {
  return call('set_message_pinned', args)
}

export function setMessagePriority(args: {
  accountId: string
  folder: string
  uid: number
  priority?: string | null
}): Promise<void> {
  return call('set_message_priority', args)
}

export function setMessageRead(args: {
  accountId: string
  folder: string
  uid: number
  read: boolean
}): Promise<void> {
  return call('set_message_read', args)
}

export function setMessageReminder(args: {
  accountId: string
  folder: string
  uid: number
  remindAt?: number | null
}): Promise<void> {
  return call('set_message_reminder', args)
}

export function respondMdnRequest(args: {
  accountId: string
  folder: string
  uid: number
  decline: boolean
  automatic: boolean
}): Promise<void> {
  return call('respond_mdn_request', args)
}

export function getReceiptStatus(args: {
  accountId: string
  messageId: string
}): Promise<SentReceiptStatus | null> {
  return call('get_receipt_status', args)
}

export function searchEmails(args: {
  query: string
  scope?: SearchScope | null
  filters?: SearchFilters | null
}): Promise<SearchHit[]> {
  return call('search_emails', args)
}

export function searchImapServer(args: {
  accountId: string
  folder: string
  query: string
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('search_imap_server', args)
}

export function searchImapServerOlder(args: {
  accountId: string
  folder: string
  query: string
  beforeUid: number
  limit: number
}): Promise<EmailEnvelope[]> {
  return call('search_imap_server_older', args)
}

export function checkMailNow(): Promise<void> {
  return call('check_mail_now')
}

export function getTotalUnread(): Promise<number> {
  return call('get_total_unread')
}

export function getUnreadCountsByAccount(): Promise<Record<string, number>> {
  return call('get_unread_counts_by_account')
}

export function downloadEmailAttachment(args: {
  accountId: string
  folder: string
  uid: number
  partId: number
}): Promise<number[]> {
  return call('download_email_attachment', args)
}

/**
 * Every image part of a message that the HTML body can reference by
 * `cid:` (#471), bytes included, in one round-trip.
 *
 * `pgpPassphrase` is only sent for encrypted messages — an empty
 * string routes the backend through the keychain ("Unlock
 * automatically"); omit it entirely for plaintext mail.
 */
export function fetchInlineImages(args: {
  accountId: string
  folder: string
  uid: number
  pgpPassphrase?: string
}): Promise<InlineImagePart[]> {
  return call('fetch_inline_images', args)
}

export function getAttachmentPreviews(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<AttachmentPreviewView[]> {
  return call('get_attachment_previews', args)
}

export function putAttachmentPreview(args: {
  accountId: string
  folder: string
  uid: number
  partId: number
  mime: string
  base64: string
}): Promise<void> {
  return call('put_attachment_preview', args)
}

export function parseEmlFile(args: { path: string }): Promise<Email> {
  return call('parse_eml_file', args)
}

/** `fetchInlineImages` for the account-less `.eml` popout (#471). */
export function parseEmlFileInlineImages(args: {
  path: string
}): Promise<InlineImagePart[]> {
  return call('parse_eml_file_inline_images', args)
}

export function checkUrls(args: { urls: string[] }): Promise<LinkVerdict[]> {
  return call('check_urls', args)
}

export function debugLinkCheck(args: { url: string }): Promise<unknown> {
  return call('debug_link_check', args)
}

export function getLinkCheckStatus(): Promise<UrlhausStatus> {
  return call('get_link_check_status')
}

export function refreshUrlhausNow(): Promise<number> {
  return call('refresh_urlhaus_now')
}
