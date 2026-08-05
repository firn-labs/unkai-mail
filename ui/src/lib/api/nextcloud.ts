/**
 * Nextcloud account plumbing plus Files and Shares: Login Flow v2,
 * DAV account setup, capabilities, file browsing/transfer, shares.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  FileEntry,
  LoginFlowInit,
  NextcloudAccount,
  NextcloudGroupView,
  NextcloudShareResult,
  NextcloudShareRow,
  NextcloudUserLookup,
  TrustedCert,
} from './types'

export function startNextcloudLogin(args: {
  serverUrl: string
  trustedCerts?: TrustedCert[] | null
}): Promise<LoginFlowInit> {
  return call('start_nextcloud_login', args)
}

export function pollNextcloudLogin(args: {
  pollEndpoint: string
  pollToken: string
  trustedCerts?: TrustedCert[] | null
}): Promise<NextcloudAccount | null> {
  return call('poll_nextcloud_login', args)
}

export function getNextcloudAccounts(): Promise<NextcloudAccount[]> {
  return call('get_nextcloud_accounts')
}

export function removeNextcloudAccount(args: { id: string }): Promise<void> {
  return call('remove_nextcloud_account', args)
}

export function addDavAccount(args: {
  displayName: string
  serverUrl: string
  username: string
  password: string
  useContacts: boolean
  useCalendars: boolean
  trustedCerts?: TrustedCert[] | null
}): Promise<NextcloudAccount> {
  return call('add_dav_account', args)
}

export function addLocalDavAccount(args: {
  displayName: string
  useContacts: boolean
  useCalendars: boolean
}): Promise<NextcloudAccount> {
  return call('add_local_dav_account', args)
}

export function updateNextcloudAccountTrustedCerts(args: {
  ncId: string
  trustedCerts: TrustedCert[]
}): Promise<NextcloudAccount> {
  return call('update_nextcloud_account_trusted_certs', args)
}

export function refreshNextcloudCapabilities(args: { ncId: string }): Promise<NextcloudAccount> {
  return call('refresh_nextcloud_capabilities', args)
}

export function getNextcloudUserEmail(args: { ncId: string }): Promise<string | null> {
  return call('get_nextcloud_user_email', args)
}

export function findNextcloudUserByEmail(args: {
  ncId: string
  email: string
}): Promise<NextcloudUserLookup | null> {
  return call('find_nextcloud_user_by_email', args)
}

export function listNextcloudGroups(): Promise<NextcloudGroupView[]> {
  return call('list_nextcloud_groups')
}

export function listNextcloudFiles(args: { ncId: string; path: string }): Promise<FileEntry[]> {
  return call('list_nextcloud_files', args)
}

export function createNextcloudDirectory(args: { ncId: string; path: string }): Promise<void> {
  return call('create_nextcloud_directory', args)
}

export function downloadNextcloudFile(args: { ncId: string; path: string }): Promise<number[]> {
  return call('download_nextcloud_file', args)
}

export function uploadToNextcloud(args: {
  ncId: string
  path: string
  data: number[]
  contentType?: string | null
}): Promise<string> {
  return call('upload_to_nextcloud', args)
}

export function nextcloudFilePreview(args: {
  ncId: string
  path: string
  size?: number | null
}): Promise<number[] | null> {
  return call('nextcloud_file_preview', args)
}

export function listNextcloudShares(args: { ncId: string }): Promise<NextcloudShareRow[]> {
  return call('list_nextcloud_shares', args)
}

export function createNextcloudShare(args: {
  ncId: string
  path: string
  password?: string | null
  label?: string | null
  permissions?: number | null
  expireDate?: string | null
}): Promise<NextcloudShareResult> {
  return call('create_nextcloud_share', args)
}

export function deleteNextcloudShare(args: { ncId: string; shareId: string }): Promise<void> {
  return call('delete_nextcloud_share', args)
}

export function updateNextcloudShare(args: {
  ncId: string
  shareId: string
  password?: string | null
  permissions?: number | null
  expireDate?: string | null
}): Promise<void> {
  return call('update_nextcloud_share', args)
}

export function updateNextcloudShareLabel(args: {
  ncId: string
  shareId: string
  label: string
}): Promise<void> {
  return call('update_nextcloud_share_label', args)
}
