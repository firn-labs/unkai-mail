/**
 * Nextcloud Notes: CRUD + sync.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  Note,
} from './types'

export function listNextcloudNotes(args: { ncId: string }): Promise<Note[]> {
  return call('list_nextcloud_notes', args)
}

export function getNextcloudNote(args: { ncId: string; noteId: number }): Promise<Note> {
  return call('get_nextcloud_note', args)
}

export function createNextcloudNote(args: {
  ncId: string
  title: string
  content: string
  category: string
}): Promise<Note> {
  return call('create_nextcloud_note', args)
}

export function updateNextcloudNote(args: {
  ncId: string
  noteId: number
  etag: string
  title?: string | null
  content?: string | null
  category?: string | null
  favorite?: boolean | null
}): Promise<Note> {
  return call('update_nextcloud_note', args)
}

export function deleteNextcloudNote(args: { ncId: string; noteId: number }): Promise<void> {
  return call('delete_nextcloud_note', args)
}

export function syncNextcloudNotes(args: { ncId: string }): Promise<Note[]> {
  return call('sync_nextcloud_notes', args)
}
