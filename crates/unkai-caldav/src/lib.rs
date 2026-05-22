//! Unkai CalDAV — calendar sync via the CalDAV protocol.
//!
//! Provides calendar event retrieval and sync with CalDAV servers,
//! including Nextcloud Calendar. Mirrors the architecture of
//! `unkai-carddav` (see its top-level comment): hand-rolled DAV
//! requests with `reqwest` + `quick-xml`, RFC 6578 `sync-collection`
//! for incremental sync, and a pure-parser `ical` module that turns
//! `text/calendar` bodies into flat `CalendarEvent`s.
//!
//! # Scope
//!
//! - Discovery + sync-collection + calendar-multiget (read-only).
//! - One `CalendarEvent` row per VEVENT in the cache — masters and
//!   `RECURRENCE-ID` overrides land as separate rows sharing a UID.
//!   The [`expand`] module then turns a master + overrides + a
//!   date window into concrete occurrences for the UI.
//! - UTC, all-day, and named-TZID events (via `chrono-tz`) all
//!   resolve accurately. Only unknown TZIDs and DST-gap edge cases
//!   fall back to UTC (logged at `warn`).
//! - Write path covers VEVENT create / update / delete via PUT /
//!   DELETE with `If-Match` etags (RFC 5545 + RFC 4918 §10.5). The
//!   editor builds a `CalendarEvent`, [`ical::build_ics`] renders it,
//!   and [`write::create_event`] / [`write::update_event`] PUTs it.

pub mod calendars;
pub mod client;
pub mod discovery;
pub mod expand;
pub mod freebusy;
pub mod ical;
pub mod sync;
pub mod write;
mod xml_util;

pub use calendars::{create_calendar, delete_calendar, update_calendar};
pub use client::probe_calendar_writable;
pub use discovery::{Calendar, list_calendars};
pub use expand::expand_event;
pub use freebusy::{BusyKind, BusyPeriod, nc_principal_home, query_free_busy};
pub use ical::{build_ics, parse_ics};
pub use sync::{CalendarSyncDelta, RawEvent, sync_calendar};
pub use write::{WriteOutcome, create_event, delete_event, delete_event_silent, update_event};
