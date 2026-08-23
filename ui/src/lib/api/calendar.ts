/**
 * Calendar and scheduling: events (live + cached), calendar CRUD and
 * visibility, invites/RSVP, reminders, availability, and geocoding.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  AttendeeAvailability,
  CalendarEvent,
  CalendarEventInput,
  CalendarSummary,
  GeocodeResult,
  ImportCalendarReport,
  InviteSummary,
  NextcloudMapsCapability,
  SyncCalendarsReport,
  SyncStatus,
} from './types'

export function createCalendarEvent(args: {
  calendarId: string
  input: CalendarEventInput
}): Promise<CalendarEvent> {
  return call('create_calendar_event', args)
}

export function updateCalendarEvent(args: {
  eventId: string
  input: CalendarEventInput
}): Promise<CalendarEvent> {
  return call('update_calendar_event', args)
}

export function deleteCalendarEvent(args: { eventId: string }): Promise<void> {
  return call('delete_calendar_event', args)
}

export function getCachedCalendars(args: { ncId: string }): Promise<CalendarSummary[]> {
  return call('get_cached_calendars', args)
}

export function getCachedEvents(args: {
  calendarIds: string[]
  rangeStart: string
  rangeEnd: string
}): Promise<CalendarEvent[]> {
  return call('get_cached_events', args)
}

export function getCalendarsSyncStatus(args: { ncId: string }): Promise<SyncStatus> {
  return call('get_calendars_sync_status', args)
}

export function syncNextcloudCalendars(args: { ncId: string }): Promise<SyncCalendarsReport> {
  return call('sync_nextcloud_calendars', args)
}

export function syncCalendarById(args: { calendarId: string }): Promise<void> {
  return call('sync_calendar_by_id', args)
}

export function listNextcloudCalendars(args: { ncId: string }): Promise<CalendarSummary[]> {
  return call('list_nextcloud_calendars', args)
}

export function createNextcloudCalendar(args: {
  ncId: string
  displayName: string
  color?: string | null
}): Promise<CalendarSummary> {
  return call('create_nextcloud_calendar', args)
}

export function deleteNextcloudCalendar(args: { calendarId: string }): Promise<void> {
  return call('delete_nextcloud_calendar', args)
}

export function updateNextcloudCalendar(args: {
  calendarId: string
  displayName?: string | null
  color?: string | null
}): Promise<void> {
  return call('update_nextcloud_calendar', args)
}

export function setNextcloudCalendarHidden(args: {
  calendarId: string
  hidden: boolean
}): Promise<void> {
  return call('set_nextcloud_calendar_hidden', args)
}

export function setNextcloudCalendarMuted(args: {
  calendarId: string
  muted: boolean
}): Promise<void> {
  return call('set_nextcloud_calendar_muted', args)
}

export function parseEventInvite(args: { bytes: number[] }): Promise<InviteSummary> {
  return call('parse_event_invite', args)
}

export function respondToInvite(args: {
  calendarId: string
  rawIcs: string
  partstat: string
  attendeeHint?: string | null
}): Promise<void> {
  return call('respond_to_invite', args)
}

export function rsvpExistingEvent(args: {
  eventId: string
  partstat: string
  attendeeHint?: string | null
}): Promise<void> {
  return call('rsvp_existing_event', args)
}

export function getRsvpResponse(args: { uid: string }): Promise<string | null> {
  return call('get_rsvp_response', args)
}

export function getEventPartstatForUser(args: {
  uid: string
  attendeeHint?: string | null
}): Promise<string | null> {
  return call('get_event_partstat_for_user', args)
}

export function isEventInCalendar(args: { uid: string }): Promise<boolean> {
  return call('is_event_in_calendar', args)
}

export function isInviteCancelled(args: { uid: string }): Promise<boolean> {
  return call('is_invite_cancelled', args)
}

export function recordCancelledInvite(args: { uid: string }): Promise<void> {
  return call('record_cancelled_invite', args)
}

export function dismissCancelledEvent(args: { uid: string }): Promise<void> {
  return call('dismiss_cancelled_event', args)
}

export function dismissEventReminder(args: { uid: string }): Promise<void> {
  return call('dismiss_event_reminder', args)
}

export function snoozeEventReminder(args: { uid: string; snoozeUntilIso: string }): Promise<void> {
  return call('snooze_event_reminder', args)
}

export function getAttendeeAvailability(args: {
  ncId: string
  attendeeEmails: string[]
  rangeStart: string
  rangeEnd: string
}): Promise<AttendeeAvailability[]> {
  return call('get_attendee_availability', args)
}

export function downloadCalendarFromMessage(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<number[] | null> {
  return call('download_calendar_from_message', args)
}

export function parseIcsFile(args: { path: string }): Promise<CalendarEvent[]> {
  return call('parse_ics_file', args)
}

export function importCalendarFile(args: {
  calendarId: string
  path: string
}): Promise<ImportCalendarReport> {
  return call('import_calendar_file', args)
}

export function geocodeSearch(args: {
  query: string
  lang?: string | null
}): Promise<GeocodeResult[]> {
  return call('geocode_search', args)
}

export function detectNcMaps(args: { ncId: string }): Promise<NextcloudMapsCapability> {
  return call('detect_nc_maps', args)
}
