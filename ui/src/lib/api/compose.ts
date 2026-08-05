/**
 * Outgoing mail: send, drafts, and the persistent outbox queue.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  DraftReplaceSource,
  OutboxRowDto,
  OutboxSourceRef,
  OutgoingEmail,
  RepliedToRef,
  SavedDraft,
} from './types'

export function sendEmail(args: {
  accountId: string
  email: OutgoingEmail
  repliedTo?: RepliedToRef | null
  outboxSource?: OutboxSourceRef | null
  pgpPassphrase?: string | null
}): Promise<number> {
  return call('send_email', args)
}

export function saveDraft(args: {
  accountId: string
  email: OutgoingEmail
  replaceSource?: DraftReplaceSource | null
}): Promise<SavedDraft> {
  return call('save_draft', args)
}

export function expungeDraftAfterSend(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<void> {
  return call('expunge_draft_after_send', args)
}

export function tombstoneDraftForExpunge(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<void> {
  return call('tombstone_draft_for_expunge', args)
}

export function listOutbox(args: { accountId: string }): Promise<OutboxRowDto[]> {
  return call('list_outbox', args)
}

export function listAllOutbox(): Promise<OutboxRowDto[]> {
  return call('list_all_outbox')
}

export function countOutbox(): Promise<number> {
  return call('count_outbox')
}

export function countOutboxByAccount(): Promise<Record<string, number>> {
  return call('count_outbox_by_account')
}

export function deleteOutboxEntry(args: { id: number }): Promise<void> {
  return call('delete_outbox_entry', args)
}

export function editOutboxEntry(args: { id: number }): Promise<OutboxRowDto> {
  return call('edit_outbox_entry', args)
}

export function retryOutboxEntry(args: { id: number }): Promise<void> {
  return call('retry_outbox_entry', args)
}

export function retryOutboxEntryWithPassphrase(args: {
  id: number
  pgpPassphrase: string
}): Promise<void> {
  return call('retry_outbox_entry_with_passphrase', args)
}
