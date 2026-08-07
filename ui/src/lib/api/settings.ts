/**
 * App settings, settings-sync bundles, custom themes, the encrypted
 * vault (FIDO/passphrase unlock, wipe policy), and the MCP server.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  AppSettings,
  CustomTheme,
  DatabaseStatusView,
  FidoStatusView,
  McpServerStatus,
  McpToolView,
  SettingsSyncStateView,
  WipePolicyView,
} from './types'

export function getAppSettings(): Promise<AppSettings> {
  return call('get_app_settings')
}

export function updateAppSettings(args: { newSettings: AppSettings }): Promise<void> {
  return call('update_app_settings', args)
}

export function notifySettingsChanged(args: {
  localStorage: Record<string, unknown>
}): Promise<void> {
  return call('notify_settings_changed', args)
}

export function setSettingsSyncTarget(args: { targetNcId?: string | null }): Promise<void> {
  return call('set_settings_sync_target', args)
}

export function getSettingsSyncState(): Promise<SettingsSyncStateView> {
  return call('get_settings_sync_state')
}

/**
 * #477 — the backend opens the native "Save As" dialog itself and
 * writes the bundle; no filesystem path crosses the IPC boundary.
 * Resolves to the chosen path, or `null` when the user cancels.
 */
export function exportSettingsBundle(args: {
  localStorage: Record<string, string>
}): Promise<string | null> {
  return call('export_settings_bundle', args)
}

/**
 * #477 — the backend opens the native file picker itself, reads the
 * chosen bundle, and applies it. Resolves to the imported path plus
 * the bundle's localStorage portion (for the caller to mirror into
 * `localStorage`), or `null` when the user cancels.
 */
export function importSettingsBundle(): Promise<{
  path: string
  localStorage: Record<string, string>
} | null> {
  return call('import_settings_bundle')
}

export function ncProbeSettingsBundle(args: { ncId: string }): Promise<string | null> {
  return call('nc_probe_settings_bundle', args)
}

export function ncRestoreSettingsBundle(args: { ncId: string }): Promise<Record<string, string>> {
  return call('nc_restore_settings_bundle', args)
}

export function importCustomTheme(args: {
  sourcePath: string
  label?: string | null
}): Promise<CustomTheme> {
  return call('import_custom_theme', args)
}

export function removeCustomTheme(args: { id: string }): Promise<void> {
  return call('remove_custom_theme', args)
}

export function setLogoStyle(args: { style: string }): Promise<void> {
  return call('set_logo_style', args)
}

export function setWipePolicy(args: { policy: WipePolicyView }): Promise<void> {
  return call('set_wipe_policy', args)
}

export function getWipePolicy(): Promise<WipePolicyView> {
  return call('get_wipe_policy')
}

export function databaseStatus(): Promise<DatabaseStatusView> {
  return call('database_status')
}

export function unlockWithPassphrase(args: { passphrase: string }): Promise<void> {
  return call('unlock_with_passphrase', args)
}

export function unlockWithPrf(args: {
  credentialIdB64: string
  prfOutputB64: string
}): Promise<void> {
  return call('unlock_with_prf', args)
}

export function fidoEnroll(args: {
  credentialIdB64: string
  saltB64: string
  prfOutputB64: string
  label: string
}): Promise<void> {
  return call('fido_enroll', args)
}

export function fidoEnrollPassphrase(args: { passphrase: string; label: string }): Promise<void> {
  return call('fido_enroll_passphrase', args)
}

export function fidoGenerateSalt(): Promise<string> {
  return call('fido_generate_salt')
}

export function fidoRemove(args: { credentialIdB64: string }): Promise<void> {
  return call('fido_remove', args)
}

export function fidoStatus(): Promise<FidoStatusView> {
  return call('fido_status')
}

export function fidoVerifyPassphrase(args: { passphrase: string }): Promise<boolean> {
  return call('fido_verify_passphrase', args)
}

export function fidoVerifyPrf(args: {
  credentialIdB64: string
  prfOutputB64: string
}): Promise<boolean> {
  return call('fido_verify_prf', args)
}

export function enableFidoOnlyMode(): Promise<void> {
  return call('enable_fido_only_mode')
}

export function disableFidoOnlyMode(): Promise<void> {
  return call('disable_fido_only_mode')
}

export function mcpGenerateToken(): Promise<string> {
  return call('mcp_generate_token')
}

export function mcpListTools(): Promise<McpToolView[]> {
  return call('mcp_list_tools')
}

export function mcpRevokeToken(): Promise<void> {
  return call('mcp_revoke_token')
}

export function mcpServerStatus(): Promise<McpServerStatus> {
  return call('mcp_server_status')
}

export function mcpTokenStatus(): Promise<boolean> {
  return call('mcp_token_status')
}
