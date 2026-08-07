/**
 * Desktop-shell integration: native notifications, window/app
 * lifecycle, external openers (Office/PDF/print/URL), mailto/file
 * handoff, fonts, and the dialog-paired attachment save (#477).
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  OfficeOpenResult,
  PdfOpenResult,
} from './types'

export function sendNativeNotification(args: {
  title: string
  body: string
  accountId?: string | null
  folder?: string | null
  uid?: number | null
}): Promise<boolean> {
  return call('send_native_notification', args)
}

export function getNotificationIconPath(): Promise<string> {
  return call('get_notification_icon_path')
}

export function showMainWindowCmd(): Promise<void> {
  return call('show_main_window_cmd')
}

export function quitApp(): Promise<void> {
  return call('quit_app')
}

export function restartApp(): Promise<void> {
  return call('restart_app')
}

export function openUrl(args: { url: string }): Promise<void> {
  return call('open_url', args)
}

export function openDefaultAppsSettings(): Promise<void> {
  return call('open_default_apps_settings')
}

export function takePendingMailtoUrls(): Promise<string[]> {
  return call('take_pending_mailto_urls')
}

export function takePendingFileToOpen(): Promise<string | null> {
  return call('take_pending_file_to_open')
}

export function listSystemFonts(): Promise<string[]> {
  return call('list_system_fonts')
}

export function printAttachment(args: { fileName: string; bytes: number[] }): Promise<void> {
  return call('print_attachment', args)
}

export function officeOpenAttachment(args: {
  ncId: string
  filename: string
  data: number[]
  contentType?: string | null
}): Promise<OfficeOpenResult> {
  return call('office_open_attachment', args)
}

export function officeCloseAttachment(args: { ncId: string; tempPath: string }): Promise<void> {
  return call('office_close_attachment', args)
}

export function officeSweepTemp(args: { ncId: string }): Promise<number> {
  return call('office_sweep_temp', args)
}

export function pdfOpenAttachment(args: {
  ncId: string
  filename: string
  data: number[]
  contentType?: string | null
}): Promise<PdfOpenResult> {
  return call('pdf_open_attachment', args)
}

export function pdfCloseAttachment(args: { ncId: string; tempPath: string }): Promise<void> {
  return call('pdf_close_attachment', args)
}

/**
 * #477 — attachment Download with the "Save As" dialog and the
 * fetch + write on the Rust side; no filesystem path (and no
 * multi-MB byte array) crosses the IPC boundary. `pgpPassphrase`
 * non-null routes through the decrypt-aware fetch (empty string =
 * keychain auto-unlock). Resolves to the chosen path, or `null`
 * when the user cancels.
 */
export function saveAttachmentAs(args: {
  accountId: string
  folder: string
  uid: number
  partId: number
  fileName: string
  pgpPassphrase?: string | null
}): Promise<string | null> {
  return call('save_attachment_as', args)
}
