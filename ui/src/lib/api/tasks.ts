/**
 * Nextcloud Tasks: task lists, tasks, and sync.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  SyncStatus,
  Task,
  TaskList,
} from './types'

export function listNextcloudTaskLists(args: { ncId: string }): Promise<TaskList[]> {
  return call('list_nextcloud_task_lists', args)
}

export function listNextcloudTasks(args: { ncId: string }): Promise<Task[]> {
  return call('list_nextcloud_tasks', args)
}

export function createNextcloudTask(args: {
  ncId: string
  listId: string
  summary: string
  description?: string | null
  dueUnix?: number | null
  dueTz?: string | null
  priority?: number | null
  url?: string | null
}): Promise<Task> {
  return call('create_nextcloud_task', args)
}

export function updateNextcloudTask(args: {
  ncId: string
  listId: string
  uid: string
  etag: string
  summary?: string | null
  description?: string | null
  status?: string | null
  priority?: number | null
  dueUnix?: number | null
  dueTz?: string | null
  clearDue?: boolean | null
  completedUnix?: number | null
  clearCompleted?: boolean | null
  url?: string | null
  categories?: string[] | null
}): Promise<Task> {
  return call('update_nextcloud_task', args)
}

export function deleteNextcloudTask(args: {
  ncId: string
  listId: string
  uid: string
}): Promise<void> {
  return call('delete_nextcloud_task', args)
}

export function createNextcloudTaskFromMail(args: {
  ncId: string
  listId: string
  mailAccountId: string
  folder: string
  uid: number
  subject: string
  from: string
}): Promise<Task> {
  return call('create_nextcloud_task_from_mail', args)
}

export function syncNextcloudTasks(args: { ncId: string; listId: string }): Promise<Task[]> {
  return call('sync_nextcloud_tasks', args)
}

export function syncNextcloudTaskLists(args: { ncId: string }): Promise<TaskList[]> {
  return call('sync_nextcloud_task_lists', args)
}

export function setNextcloudTaskListHidden(args: {
  taskListId: string
  hidden: boolean
}): Promise<void> {
  return call('set_nextcloud_task_list_hidden', args)
}

export function setNextcloudTaskListMuted(args: {
  taskListId: string
  muted: boolean
}): Promise<void> {
  return call('set_nextcloud_task_list_muted', args)
}

export function getTasksSyncStatus(args: { ncId: string }): Promise<SyncStatus> {
  return call('get_tasks_sync_status', args)
}
