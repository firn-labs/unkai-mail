/**
 * Mail-account management: setup wizard discovery, connection tests,
 * certificate trust probing, and CRUD on the account list.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  Account,
  DiscoveredAccount,
  ProbedCert,
  ProviderPreset,
  TrustedCert,
} from './types'

export function addAccount(args: { account: Account; password: string }): Promise<void> {
  return call('add_account', args)
}

export function updateAccount(args: { account: Account }): Promise<void> {
  return call('update_account', args)
}

export function removeAccount(args: { id: string }): Promise<void> {
  return call('remove_account', args)
}

export function getAccounts(): Promise<Account[]> {
  return call('get_accounts')
}

export function setAccountPassword(args: { id: string; password: string }): Promise<void> {
  return call('set_account_password', args)
}

export function discoverAccountSettings(args: {
  email: string
}): Promise<DiscoveredAccount | null> {
  return call('discover_account_settings', args)
}

export function listProviderPresets(): Promise<ProviderPreset[]> {
  return call('list_provider_presets')
}

export function probeServerCertificate(args: { host: string; port: number }): Promise<ProbedCert> {
  return call('probe_server_certificate', args)
}

export function testConnection(args: {
  host: string
  port: number
  username: string
  password: string
  trustedCerts?: TrustedCert[] | null
}): Promise<string> {
  return call('test_connection', args)
}

export function testJmapConnection(args: {
  jmapUrl: string
  username: string
  password: string
}): Promise<string> {
  return call('test_jmap_connection', args)
}

export function detectJmap(args: { host: string }): Promise<string | null> {
  return call('detect_jmap', args)
}
