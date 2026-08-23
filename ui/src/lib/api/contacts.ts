/**
 * Contacts and their groupings: CardDAV-synced contacts, contact
 * groups, categories, and mailing lists.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  AddressbookSummary,
  Contact,
  ContactCategoryView,
  ContactGroupView,
  ContactInput,
  ContactPhoto,
  ImportContactsReport,
  MailingListView,
  SyncContactsReport,
  SyncStatus,
} from './types'

export function createContact(args: {
  ncId: string
  addressbookUrl: string
  addressbookName: string
  input: ContactInput
}): Promise<Contact> {
  return call('create_contact', args)
}

export function updateContact(args: { contactId: string; input: ContactInput }): Promise<Contact> {
  return call('update_contact', args)
}

export function deleteContact(args: { contactId: string }): Promise<void> {
  return call('delete_contact', args)
}

export function importContactsFile(args: {
  ncId: string
  addressbookUrl: string
  addressbookName: string
  path: string
}): Promise<ImportContactsReport> {
  return call('import_contacts_file', args)
}

export function getContacts(args: { ncId?: string | null } = {}): Promise<Contact[]> {
  return call('get_contacts', args)
}

export function searchContacts(args: { query: string; limit: number }): Promise<Contact[]> {
  return call('search_contacts', args)
}

export function getContactPhoto(args: { contactId: string }): Promise<ContactPhoto | null> {
  return call('get_contact_photo', args)
}

export function syncNextcloudContacts(args: { ncId: string }): Promise<SyncContactsReport> {
  return call('sync_nextcloud_contacts', args)
}

export function getContactsSyncStatus(args: { ncId: string }): Promise<SyncStatus> {
  return call('get_contacts_sync_status', args)
}

export function createContactGroup(args: {
  ncId: string
  addressbookUrl: string
  addressbookName: string
  displayName: string
  memberUids: string[]
}): Promise<ContactGroupView> {
  return call('create_contact_group', args)
}

export function updateContactGroup(args: {
  groupId: string
  displayName?: string | null
  memberUids?: string[] | null
}): Promise<ContactGroupView> {
  return call('update_contact_group', args)
}

export function deleteContactGroup(args: { groupId: string }): Promise<void> {
  return call('delete_contact_group', args)
}

export function listContactGroups(): Promise<ContactGroupView[]> {
  return call('list_contact_groups')
}

export function setContactGroupEmoji(args: {
  groupId: string
  emoji?: string | null
}): Promise<void> {
  return call('set_contact_group_emoji', args)
}

export function setContactGroupHidden(args: { groupId: string; hidden: boolean }): Promise<void> {
  return call('set_contact_group_hidden', args)
}

export function addContactToCategory(args: { contactId: string; category: string }): Promise<void> {
  return call('add_contact_to_category', args)
}

export function removeContactFromCategory(args: {
  contactId: string
  category: string
}): Promise<void> {
  return call('remove_contact_from_category', args)
}

export function listContactCategories(): Promise<ContactCategoryView[]> {
  return call('list_contact_categories')
}

export function deleteContactCategory(args: { name: string }): Promise<void> {
  return call('delete_contact_category', args)
}

export function renameContactCategory(args: { old: string; new: string }): Promise<void> {
  return call('rename_contact_category', args)
}

export function setCategoryUseAsMailingList(args: {
  name: string
  enabled: boolean
}): Promise<void> {
  return call('set_category_use_as_mailing_list', args)
}

export function listMailingLists(): Promise<MailingListView[]> {
  return call('list_mailing_lists')
}

export function renameMailingList(args: { id: string; newName: string }): Promise<void> {
  return call('rename_mailing_list', args)
}

export function setMailingListEmoji(args: { id: string; emoji?: string | null }): Promise<void> {
  return call('set_mailing_list_emoji', args)
}

export function setMailingListHidden(args: { id: string; hidden: boolean }): Promise<void> {
  return call('set_mailing_list_hidden', args)
}

export function listNextcloudAddressbooks(args: { ncId: string }): Promise<AddressbookSummary[]> {
  return call('list_nextcloud_addressbooks', args)
}
