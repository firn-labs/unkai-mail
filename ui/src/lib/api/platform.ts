/**
 * Desktop-platform affordances that aren't backend commands (#473):
 * native file dialogs, OS notifications via the plugin, login-item
 * autostart, and the custom-protocol asset URLs (`contact-photo://`,
 * `unkai-logo://`) that `convertFileSrc` turns into webview-loadable
 * URLs.
 *
 * Same rationale as `api/core`: components stay free of direct
 * `@tauri-apps/*` imports so the desktop-only surface is enumerable in
 * one file — this module IS the list of what a non-desktop build would
 * have to re-provide.
 */

import { convertFileSrc } from '@tauri-apps/api/core'
import { open as dialogOpen, type OpenDialogOptions } from '@tauri-apps/plugin-dialog'
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
  type Options as NotificationOptions,
} from '@tauri-apps/plugin-notification'
import {
  enable as autostartEnable,
  disable as autostartDisable,
  isEnabled as autostartIsEnabled,
} from '@tauri-apps/plugin-autostart'

/* ── file dialogs ──────────────────────────────────────────────── */
//
// Open-picker only. There is deliberately no save-dialog wrapper:
// every "save to disk" flow pairs the dialog with the file IO on
// the Rust side (#477 — `save_attachment_as`,
// `export_settings_bundle`), so the webview never handles a
// writable filesystem path.

export function openFileDialog(options?: OpenDialogOptions): Promise<string | string[] | null> {
  return dialogOpen(options)
}

/* ── notifications (plugin path — the richer per-OS toast path is
 *    the `send_native_notification` backend command in api/system) ── */

export function notificationsPermissionGranted(): Promise<boolean> {
  return isPermissionGranted()
}

export function requestNotificationsPermission(): Promise<NotificationPermission> {
  return requestPermission()
}

export function showNotification(options: NotificationOptions | string): void {
  sendNotification(options)
}

/* ── autostart ─────────────────────────────────────────────────── */

export function enableAutostart(): Promise<void> {
  return autostartEnable()
}

export function disableAutostart(): Promise<void> {
  return autostartDisable()
}

export function isAutostartEnabled(): Promise<boolean> {
  return autostartIsEnabled()
}

/* ── custom-protocol asset URLs ────────────────────────────────── */

export function assetUrl(filePath: string, protocol?: string): string {
  return convertFileSrc(filePath, protocol)
}
