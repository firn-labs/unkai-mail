//! Calendar + event cache and sync-state persistence.
//!
//! The shapes here mirror the CalDAV layer's outputs, but live in
//! their own structs (`CalendarRow`, `CalendarEventRow`) so this
//! crate doesn't have to depend on `unkai-caldav`. The Tauri layer
//! maps between the two — the same pattern we use for contacts.
//!
//! # What gets stored where
//!
//! - `calendars` — one row per remote calendar. Carries the RFC 6578
//!   `sync_token` and Nextcloud's `ctag` so the next app launch can
//!   do an incremental sync instead of a full re-fetch.
//!
//! - `calendar_events` — one row per VEVENT. A single CalDAV href can
//!   carry a master plus several recurrence-id overrides; we unfold
//!   those into separate rows that all share `(calendar_id, uid)` and
//!   are distinguished by `recurrence_id` (NULL for the master, the
//!   original occurrence's epoch seconds for an override).
//!
//! # Why we keep `ics_raw`
//!
//! Same reasoning as contacts: if we later surface more iCalendar
//! fields (ATTENDEE, ORGANIZER, VALARM…) we can re-extract them from
//! the cached blob without re-syncing every event. The recurrence
//! expander in `unkai_caldav::expand` also benefits — it can
//! cross-check a row's parsed `rrule` / `exdate` against the raw
//! source if anything ever looks off.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{OptionalExtension, params};

use unkai_core::models::{CalendarEvent, EventAttendee, EventReminder};

use crate::cache::{Cache, CacheError};

/// One discovered calendar ready for upsert.
///
/// Comes from CalDAV PROPFIND in `unkai_caldav::discovery`. We take
/// only the fields that matter for the cache — the path is the stable
/// key, and `ctag` is optional because not every server surfaces it.
/// `sync_token` is **not** on this struct: discovery returns an
/// initial token but we let `apply_event_delta` write it atomically
/// alongside the first batch of events so the two stay consistent.
#[derive(Debug, Clone)]
pub struct CalendarRow {
    /// Absolute URL of the calendar collection on the server, e.g.
    /// `https://cloud.example.com/remote.php/dav/calendars/alice/personal/`.
    /// `unkai-caldav::discovery` resolves the multistatus href against
    /// the server URL before storing, so callers don't need to prefix
    /// the server origin themselves. Stable across syncs.
    pub path: String,
    pub display_name: String,
    /// Hex colour (e.g. `#2bb0ed`). Optional — some servers / user
    /// agents don't set it.
    pub color: Option<String>,
    pub ctag: Option<String>,
    /// Layer 1 visibility (Settings). When `true` the calendar is
    /// removed from the CalendarView sidebar entirely. Local-only —
    /// never synced to the server. Toggled from NextcloudSettings'
    /// per-calendar checkboxes.
    pub hidden: bool,
    /// Layer 2 visibility (sidebar swatch). When `true` the calendar
    /// row still appears in the sidebar but its events are not painted
    /// on the agenda grid. Local-only — never synced to the server.
    /// Toggled via the coloured swatch button in the CalendarView sidebar.
    pub muted: bool,
    /// CalDAV `current-user-privilege-set` says the user can only
    /// read this calendar (#236).  Mirrors the field on the
    /// discovery `Calendar` struct so a post-discovery upsert can
    /// stamp it.  When `true`, the EventEditor hides the Delete
    /// button and removes the calendar from the new-event picker.
    pub read_only: bool,
}

/// One VEVENT ready for upsert. Mirrors `unkai_caldav::RawEvent`'s
/// inner shape without the dependency.
#[derive(Debug, Clone)]
pub struct CalendarEventRow {
    /// The VEVENT UID from the iCalendar source. Shared between a
    /// master and all of its recurrence-id overrides — that's how
    /// RFC 5545 links them.
    pub uid: String,
    /// `None` for masters and non-recurring events; `Some(original
    /// occurrence start)` for a RECURRENCE-ID override.
    pub recurrence_id: Option<DateTime<Utc>>,
    /// CalDAV href this event lives at. Multiple rows can share one
    /// href (master + overrides are colocated in the same ICS blob).
    pub href: String,
    pub etag: String,
    pub summary: String,
    pub description: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub location: Option<String>,
    pub rrule: Option<String>,
    pub rdate: Vec<DateTime<Utc>>,
    pub exdate: Vec<DateTime<Utc>>,
    /// `URL` property — link associated with the event.
    pub url: Option<String>,
    /// `TRANSP` — `OPAQUE` (busy) or `TRANSPARENT` (free).
    pub transparency: Option<String>,
    /// Parsed `ATTENDEE` properties.
    pub attendees: Vec<EventAttendee>,
    /// Parsed `VALARM` blocks.
    pub reminders: Vec<EventReminder>,
    /// `GEO` latitude (RFC 5545 §3.8.1.6) — populated from the
    /// EventEditor's location-autocomplete pick (#280).
    pub latitude: Option<f64>,
    /// `GEO` longitude — pairs with `latitude`.
    pub longitude: Option<f64>,
    /// Raw `text/calendar` blob as the server returned it. Kept so
    /// future parser evolutions and the recurrence expander have a
    /// source of truth on disk.
    pub ics_raw: String,
}

/// A calendar as read back from the cache. Extends `CalendarRow` with
/// the server-side sync bookkeeping we populate during sync.
#[derive(Debug, Clone)]
pub struct CachedCalendar {
    /// App-side `{nc_account_id}::{path}` — stable across syncs;
    /// events FK on this.
    pub id: String,
    pub nextcloud_account_id: String,
    pub path: String,
    pub display_name: String,
    pub color: Option<String>,
    pub ctag: Option<String>,
    /// Last known RFC 6578 sync-collection token. Feed this back to
    /// the server on the next sync to get "what's changed since".
    pub sync_token: Option<String>,
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Layer 1 visibility (Settings). When `true` the sidebar hides
    /// the row entirely. Never round-trips to the server.
    pub hidden: bool,
    /// Layer 2 visibility (sidebar swatch). When `true` the row
    /// appears in the sidebar but its events are skipped on the grid.
    /// Never round-trips to the server.
    pub muted: bool,
    /// CalDAV-derived read-only flag (#236).  See `CalendarRow`.
    pub read_only: bool,
}

/// Sync bookmark for a single calendar. Separate from `CachedCalendar`
/// because the sync path only needs the bookkeeping bits and doesn't
/// care about display metadata.
#[derive(Debug, Clone)]
pub struct CalendarSyncState {
    pub sync_token: Option<String>,
    pub ctag: Option<String>,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// All rows the recurrence expander needs to render a date window.
///
/// Fetched in one `list_events_for_expansion` call so the Tauri layer
/// can loop over masters and match overrides against them without
/// issuing a query per master.
#[derive(Debug, Default, Clone)]
pub struct ExpansionInput {
    /// Non-recurring, non-override events whose time overlaps the
    /// requested window. Safe to return straight to the UI.
    pub singletons: Vec<CalendarEvent>,
    /// Recurring masters in the given calendars. **Not** filtered by
    /// the window — a weekly series's master row sits on the first
    /// occurrence, which is often far before the visible window even
    /// though later occurrences fall inside it.
    pub masters: Vec<CalendarEvent>,
    /// RECURRENCE-ID overrides for the given calendars. **Not**
    /// filtered by the window either — an override whose *new* start
    /// is in-window is relevant even if its original RECURRENCE-ID is
    /// outside, so we hand the expander the full set and let it
    /// decide.
    pub overrides: Vec<CalendarEvent>,
}

impl Cache {
    // ── Calendars ───────────────────────────────────────────────

    /// Reconcile the cached calendar list for a Nextcloud account
    /// against a fresh discovery result.
    ///
    /// Upserts every `rows[i]` (preserving `sync_token` on existing
    /// rows — display_name / color / ctag can drift, the token can't
    /// be recovered from discovery) and deletes any calendar that
    /// was previously cached for `nc_account_id` but no longer
    /// appears in the server's list. `ON DELETE CASCADE` on
    /// `calendar_events.calendar_id` takes care of the events.
    ///
    /// All inside one transaction so a server hiccup mid-reconcile
    /// leaves the old state intact rather than a half-applied list.
    pub fn upsert_calendars(
        &self,
        nc_account_id: &str,
        rows: &[CalendarRow],
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;

        // Insert / update every calendar in the server list. We
        // explicitly do NOT touch `sync_token` or `last_synced_at`
        // here — those are only valid after a real sync, and a
        // discovery run shouldn't pretend otherwise.
        {
            // `hidden` and `muted` are deliberately NOT in the UPDATE
            // set — both are local-only toggles the server doesn't know
            // about, so a post-sync upsert must not overwrite what the
            // user picked. On first insert we stamp whatever the row
            // carries (both default to `false` from discovery).
            //
            // `read_only` (#236) IS in the UPDATE set — discovery
            // re-runs the active write-probe (`probe_calendar_
            // writable`) on every sync, so its verdict is canonical:
            // if the user's permissions changed server-side
            // (granted or revoked), the probe reflects that and
            // the cache should follow.  Earlier we made this
            // sticky-true to compensate for unreliable
            // privilege-set / OPTIONS signals; the probe replaces
            // both, so we can trust `excluded.read_only` directly
            // again.
            let mut stmt = tx.prepare(
                "INSERT INTO calendars
                    (id, nextcloud_account_id, path, display_name, color, ctag, hidden, muted, read_only)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (nextcloud_account_id, path) DO UPDATE SET
                    display_name = excluded.display_name,
                    color        = COALESCE(excluded.color, calendars.color),
                    ctag         = COALESCE(excluded.ctag, calendars.ctag),
                    read_only    = excluded.read_only",
            )?;
            for r in rows {
                let id = format!("{nc_account_id}::{}", r.path);
                stmt.execute(params![
                    id,
                    nc_account_id,
                    r.path,
                    r.display_name,
                    r.color,
                    r.ctag,
                    r.hidden as i64,
                    r.muted as i64,
                    r.read_only as i64,
                ])?;
            }
        }

        // Prune calendars that vanished server-side. We bind the
        // full server path list as an in-memory temp join so we
        // don't have to build a variadic `NOT IN (...)` clause.
        tx.execute("CREATE TEMP TABLE _seen_paths (path TEXT PRIMARY KEY)", [])?;
        {
            let mut stmt = tx.prepare("INSERT INTO _seen_paths (path) VALUES (?1)")?;
            for r in rows {
                stmt.execute(params![r.path])?;
            }
        }
        tx.execute(
            "DELETE FROM calendars
             WHERE nextcloud_account_id = ?1
               AND path NOT IN (SELECT path FROM _seen_paths)",
            params![nc_account_id],
        )?;
        tx.execute("DROP TABLE _seen_paths", [])?;

        tx.commit()?;
        Ok(())
    }

    /// Insert a single freshly-created calendar without pruning.
    /// `upsert_calendars` is the reconcile path — it assumes the
    /// incoming list is the complete server-side truth and deletes
    /// anything missing. After a MKCALENDAR we only know about the
    /// one new collection, so we add it directly and let the next
    /// full sync fill in `ctag` / `sync_token`. Idempotent on the
    /// (nc_account_id, path) uniqueness constraint — a replay after
    /// discovery already grabbed the row is a no-op on the stored
    /// values.
    pub fn insert_calendar(
        &self,
        nc_account_id: &str,
        row: &CalendarRow,
    ) -> Result<String, CacheError> {
        let conn = self.conn()?;
        let id = format!("{nc_account_id}::{}", row.path);
        conn.execute(
            "INSERT INTO calendars
                (id, nextcloud_account_id, path, display_name, color, ctag, hidden, muted, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (nextcloud_account_id, path) DO NOTHING",
            params![
                id,
                nc_account_id,
                row.path,
                row.display_name,
                row.color,
                row.ctag,
                row.hidden as i64,
                row.muted as i64,
                row.read_only as i64,
            ],
        )?;
        Ok(id)
    }

    /// Patch an existing calendar's display name / color in place.
    /// Either argument may be `None`; both `None` is a no-op. The
    /// server-side change is driven separately via
    /// `unkai_caldav::update_calendar` — this only keeps our cache
    /// in sync so the UI paints the new value without waiting for
    /// the next full sync.
    pub fn update_calendar_metadata(
        &self,
        calendar_id: &str,
        display_name: Option<&str>,
        color: Option<&str>,
    ) -> Result<(), CacheError> {
        if display_name.is_none() && color.is_none() {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute(
            "UPDATE calendars
             SET display_name = COALESCE(?2, display_name),
                 color        = COALESCE(?3, color)
             WHERE id = ?1",
            params![calendar_id, display_name, color],
        )?;
        Ok(())
    }

    /// Layer 1 toggle — removes the calendar from the sidebar entirely.
    /// Purely client-side; never touches the server.
    pub fn set_calendar_hidden(&self, calendar_id: &str, hidden: bool) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE calendars SET hidden = ?2 WHERE id = ?1",
            params![calendar_id, hidden as i64],
        )?;
        Ok(())
    }

    /// Layer 2 toggle — keeps the calendar in the sidebar but hides its
    /// events from the agenda grid. Purely client-side; never touches
    /// the server.
    pub fn set_calendar_muted(&self, calendar_id: &str, muted: bool) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE calendars SET muted = ?2 WHERE id = ?1",
            params![calendar_id, muted as i64],
        )?;
        Ok(())
    }

    /// Drop a calendar row + all its cached events (via CASCADE on
    /// `calendar_events.calendar_id`). Called after a successful
    /// CalDAV DELETE so the sidebar forgets the collection without
    /// waiting for the next discovery sweep to prune it.
    pub fn remove_calendar(&self, calendar_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM calendars WHERE id = ?1", params![calendar_id])?;
        Ok(())
    }

    /// Flip a calendar's `read_only` flag (#236 follow-up).  Used
    /// by the CalDAV write-failure fallback in `main.rs`: when a
    /// PUT or DELETE returns 403 or 404, we treat that as the
    /// server saying "no write access" (Sabre/DAV's permission
    /// masking pattern) and stamp the calendar as read-only
    /// locally so the EventEditor stops offering Save / Delete
    /// for this and any other event on it.  Idempotent.
    pub fn set_calendar_read_only(
        &self,
        calendar_id: &str,
        read_only: bool,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE calendars SET read_only = ?2 WHERE id = ?1",
            params![calendar_id, read_only as i64],
        )?;
        Ok(())
    }

    /// All cached calendars for one Nextcloud account, alphabetised
    /// by display name.
    pub fn list_calendars(&self, nc_account_id: &str) -> Result<Vec<CachedCalendar>, CacheError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, nextcloud_account_id, path, display_name, color,
                    ctag, sync_token, last_synced_at, hidden, muted, read_only
             FROM calendars
             WHERE nextcloud_account_id = ?1
             ORDER BY display_name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![nc_account_id], |r| {
            let ts: Option<i64> = r.get(7)?;
            let hidden: i64 = r.get(8)?;
            let muted: i64 = r.get(9)?;
            let read_only: i64 = r.get(10)?;
            Ok(CachedCalendar {
                id: r.get(0)?,
                nextcloud_account_id: r.get(1)?,
                path: r.get(2)?,
                display_name: r.get(3)?,
                color: r.get(4)?,
                ctag: r.get(5)?,
                sync_token: r.get(6)?,
                last_synced_at: ts.and_then(|t| Utc.timestamp_opt(t, 0).single()),
                hidden: hidden != 0,
                muted: muted != 0,
                read_only: read_only != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Most-recent `last_synced_at` across every calendar for the
    /// given Nextcloud account, in UTC. Mirror of
    /// `latest_addressbook_sync_at` — same shape, same purpose
    /// (settings-row "synced 12m ago" chip).
    pub fn latest_calendar_sync_at(
        &self,
        nc_account_id: &str,
    ) -> Result<Option<DateTime<Utc>>, CacheError> {
        let conn = self.conn()?;
        let ts: Option<i64> = conn
            .query_row(
                "SELECT MAX(last_synced_at)
                 FROM calendars
                 WHERE nextcloud_account_id = ?1",
                params![nc_account_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(ts.and_then(|t| Utc.timestamp_opt(t, 0).single()))
    }

    /// Read the sync bookmark for a single calendar.
    pub fn get_calendar_sync_state(
        &self,
        calendar_id: &str,
    ) -> Result<Option<CalendarSyncState>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT sync_token, ctag, last_synced_at
                 FROM calendars
                 WHERE id = ?1",
                params![calendar_id],
                |r| {
                    let ts: Option<i64> = r.get(2)?;
                    Ok(CalendarSyncState {
                        sync_token: r.get(0)?,
                        ctag: r.get(1)?,
                        last_synced_at: ts.and_then(|t| Utc.timestamp_opt(t, 0).single()),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // ── Calendar events ─────────────────────────────────────────

    /// Apply one CalDAV sync delta to a single calendar.
    ///
    /// `upserts` are VEVENT rows that were added or changed;
    /// `deleted_hrefs` are resources the server reported as gone
    /// (404 in the sync-collection response). The new sync token
    /// and ctag, when provided, land on the `calendars` row in the
    /// same transaction so the bookmark never drifts away from the
    /// data it corresponds to.
    ///
    /// # Master vs. override rows
    ///
    /// A CalDAV href often contains one master VEVENT plus any
    /// number of RECURRENCE-ID overrides. We store them as separate
    /// rows keyed by `{calendar_id}::{uid}[::{recurrence_epoch}]` —
    /// that way a query in a date window returns both the base
    /// occurrence and any overrides for that window naturally.
    ///
    /// When a delete comes in we key by `href` rather than by id,
    /// since the server only tells us the resource is gone — not
    /// which of the VEVENT rows inside it existed on our side. A
    /// single `DELETE ... WHERE href = ?` cleans up the master and
    /// all overrides at once, which is the semantically correct
    /// behaviour.
    pub fn apply_event_delta(
        &self,
        calendar_id: &str,
        upserts: &[CalendarEventRow],
        deleted_hrefs: &[String],
        new_sync_token: Option<&str>,
        new_ctag: Option<&str>,
    ) -> Result<(), CacheError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = Utc::now().timestamp();

        if !deleted_hrefs.is_empty() {
            let mut stmt = tx.prepare(
                "DELETE FROM calendar_events
                 WHERE calendar_id = ?1 AND href = ?2",
            )?;
            for href in deleted_hrefs {
                stmt.execute(params![calendar_id, href])?;
            }
        }

        // When an href is re-synced we don't know up-front whether
        // the new ICS has the same mix of master/overrides as before.
        // Simplest correct behaviour: wipe the href's existing rows
        // and re-insert from `upserts`. Overrides that were removed
        // server-side therefore disappear locally too, even though
        // they don't show up in `deleted_hrefs`.
        if !upserts.is_empty() {
            let mut wipe = tx.prepare(
                "DELETE FROM calendar_events
                 WHERE calendar_id = ?1 AND href = ?2",
            )?;
            // Collect the distinct set of hrefs in this delta so we
            // only clear each once, even if the batch contains
            // master + overrides at the same href.
            let mut seen_hrefs: Vec<&str> = Vec::new();
            for e in upserts {
                if !seen_hrefs.contains(&e.href.as_str()) {
                    wipe.execute(params![calendar_id, e.href])?;
                    seen_hrefs.push(e.href.as_str());
                }
            }

            let mut stmt = tx.prepare(
                "INSERT INTO calendar_events
                    (id, calendar_id, uid, href, etag, summary, description,
                     start_utc, end_utc, location, rrule, rdate_json, exdate_json,
                     recurrence_id, ics_raw, cached_at,
                     url, transparency, attendees_json, reminders_json,
                     latitude, longitude)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            )?;
            for e in upserts {
                let id = event_row_id(calendar_id, &e.uid, e.recurrence_id);
                let rdate_json = serde_json::to_string(
                    &e.rdate.iter().map(|d| d.timestamp()).collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".into());
                let exdate_json = serde_json::to_string(
                    &e.exdate.iter().map(|d| d.timestamp()).collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".into());
                let attendees_json =
                    serde_json::to_string(&e.attendees).unwrap_or_else(|_| "[]".into());
                let reminders_json =
                    serde_json::to_string(&e.reminders).unwrap_or_else(|_| "[]".into());
                stmt.execute(params![
                    id,
                    calendar_id,
                    e.uid,
                    e.href,
                    e.etag,
                    e.summary,
                    e.description,
                    e.start.timestamp(),
                    e.end.timestamp(),
                    e.location,
                    e.rrule,
                    rdate_json,
                    exdate_json,
                    e.recurrence_id.map(|d| d.timestamp()),
                    e.ics_raw,
                    now,
                    e.url,
                    e.transparency,
                    attendees_json,
                    reminders_json,
                    e.latitude,
                    e.longitude,
                ])?;
            }
        }

        // Bump the bookmark even when the delta was empty — an empty
        // incremental run still proves "we talked to the server at
        // time T and nothing had changed", which keeps the UI from
        // showing a stale "last synced 3 hours ago".
        tx.execute(
            "UPDATE calendars
             SET sync_token     = COALESCE(?1, sync_token),
                 ctag           = COALESCE(?2, ctag),
                 last_synced_at = ?3
             WHERE id = ?4",
            params![new_sync_token, new_ctag, now, calendar_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// All events in `[range_start, range_end)` across the supplied
    /// calendars, ordered by start time ascending. Returns both
    /// masters and recurrence-id overrides as stored — callers that
    /// need concrete occurrences should go through
    /// [`Cache::list_events_for_expansion`] and `unkai_caldav::
    /// expand_event` instead, which turns RRULE/RDATE/EXDATE series
    /// into visible occurrences.
    ///
    /// Half-open interval: events whose `start_utc` falls in the
    /// window are included; the `end_utc >= range_start` condition
    /// also pulls in events that started earlier but are still
    /// ongoing across the boundary.
    pub fn list_events_in_range(
        &self,
        calendar_ids: &[String],
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, CacheError> {
        if calendar_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn()?;
        let placeholders = sql_placeholders(calendar_ids.len());
        let sql = format!(
            "SELECT {EVENT_COLUMNS}
             FROM calendar_events
             WHERE calendar_id IN ({placeholders})
               AND end_utc   >= ?{start_idx}
               AND start_utc <  ?{end_idx}
             ORDER BY start_utc ASC",
            start_idx = calendar_ids.len() + 1,
            end_idx = calendar_ids.len() + 2,
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(calendar_ids.len() + 2);
        for id in calendar_ids {
            bound.push(Box::new(id.clone()));
        }
        bound.push(Box::new(range_start.timestamp()));
        bound.push(Box::new(range_end.timestamp()));
        let param_refs: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(param_refs.as_slice(), row_to_calendar_event)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Pull everything the recurrence expander needs for a date
    /// window: in-window singletons plus *all* masters and overrides
    /// in the given calendars.
    ///
    /// Why three separate queries, not one? The three result sets
    /// have structurally different filters — singletons intersect the
    /// window, masters and overrides don't — and collapsing them
    /// into a single query forces the caller to re-classify rows
    /// client-side. Three small, named queries are easier to audit
    /// and easier to profile if the expansion ever looks slow.
    pub fn list_events_for_expansion(
        &self,
        calendar_ids: &[String],
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
    ) -> Result<ExpansionInput, CacheError> {
        if calendar_ids.is_empty() {
            return Ok(ExpansionInput::default());
        }

        let conn = self.conn()?;
        let placeholders = sql_placeholders(calendar_ids.len());

        // ── Singletons in-window ────────────────────────────────
        // Non-recurring, non-override rows. `rdate_json = '[]'` drops
        // the pure-RDATE "floating" masters — they belong in the
        // `masters` set instead so the expander can emit every RDATE.
        let singletons_sql = format!(
            "SELECT {EVENT_COLUMNS}
             FROM calendar_events
             WHERE calendar_id IN ({placeholders})
               AND rrule IS NULL
               AND recurrence_id IS NULL
               AND rdate_json = '[]'
               AND end_utc   >= ?{start_idx}
               AND start_utc <  ?{end_idx}
             ORDER BY start_utc ASC",
            start_idx = calendar_ids.len() + 1,
            end_idx = calendar_ids.len() + 2,
        );
        let singletons = self.run_event_query(
            &conn,
            &singletons_sql,
            calendar_ids,
            Some((range_start, range_end)),
        )?;

        // ── Recurring masters (all of them) ─────────────────────
        let masters_sql = format!(
            "SELECT {EVENT_COLUMNS}
             FROM calendar_events
             WHERE calendar_id IN ({placeholders})
               AND (rrule IS NOT NULL OR rdate_json != '[]')
               AND recurrence_id IS NULL
             ORDER BY start_utc ASC"
        );
        let masters = self.run_event_query(&conn, &masters_sql, calendar_ids, None)?;

        // ── All RECURRENCE-ID overrides ─────────────────────────
        let overrides_sql = format!(
            "SELECT {EVENT_COLUMNS}
             FROM calendar_events
             WHERE calendar_id IN ({placeholders})
               AND recurrence_id IS NOT NULL
             ORDER BY start_utc ASC"
        );
        let overrides = self.run_event_query(&conn, &overrides_sql, calendar_ids, None)?;

        Ok(ExpansionInput {
            singletons,
            masters,
            overrides,
        })
    }

    /// Bind `calendar_ids` into the IN-clause placeholders and, when
    /// `range` is provided, also bind the start/end timestamps.
    /// Private helper used by the three shapes of event query.
    fn run_event_query(
        &self,
        conn: &rusqlite::Connection,
        sql: &str,
        calendar_ids: &[String],
        range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Result<Vec<CalendarEvent>, CacheError> {
        let mut stmt = conn.prepare(sql)?;
        let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(calendar_ids.len() + 2);
        for id in calendar_ids {
            bound.push(Box::new(id.clone()));
        }
        if let Some((rs, re)) = range {
            bound.push(Box::new(rs.timestamp()));
            bound.push(Box::new(re.timestamp()));
        }
        let param_refs: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_calendar_event)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Drop every calendar and event we have cached for a Nextcloud
    /// account — called when the user disconnects that account.
    /// `ON DELETE CASCADE` handles the events side, so this is one
    /// statement.
    pub fn wipe_nextcloud_calendars(&self, nc_account_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM calendars WHERE nextcloud_account_id = ?1",
            params![nc_account_id],
        )?;
        Ok(())
    }

    /// Look up the (server, calendar) coordinates for an event row by
    /// app-side id. The Tauri write commands need href + etag for the
    /// PUT/DELETE preconditions, plus the parent calendar's path for
    /// constructing a fresh URL on create.
    ///
    /// Returns `Ok(None)` if the row isn't cached — callers treat that
    /// as "stale UI; refresh and try again".
    pub fn get_event_server_handle(
        &self,
        event_id: &str,
    ) -> Result<Option<CalendarEventServerHandle>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT e.uid, e.href, e.etag, e.recurrence_id, e.ics_raw,
                        e.calendar_id, c.nextcloud_account_id, c.path
                 FROM calendar_events e
                 JOIN calendars c ON c.id = e.calendar_id
                 WHERE e.id = ?1",
                params![event_id],
                |r| {
                    let recurrence_ts: Option<i64> = r.get(3)?;
                    Ok(CalendarEventServerHandle {
                        uid: r.get(0)?,
                        href: r.get(1)?,
                        etag: r.get(2)?,
                        recurrence_id: recurrence_ts.and_then(|t| Utc.timestamp_opt(t, 0).single()),
                        ics_raw: r.get(4)?,
                        calendar_id: r.get(5)?,
                        nextcloud_account_id: r.get(6)?,
                        calendar_path: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Look up the (nc account, server path) coordinates for a
    /// cached calendar by app-side id. Used by the create-event
    /// command to build the resource URL before any event row exists.
    pub fn get_calendar_server_path(
        &self,
        calendar_id: &str,
    ) -> Result<Option<(String, String)>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT nextcloud_account_id, path FROM calendars WHERE id = ?1",
                params![calendar_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Insert (or replace) a single event row outside the
    /// sync-collection delta path. Used by the create / update Tauri
    /// commands after a successful PUT to Nextcloud — we already have
    /// the server's new etag and don't want to wait for the next sync
    /// to see our own write.
    pub fn upsert_single_event(
        &self,
        calendar_id: &str,
        row: &CalendarEventRow,
    ) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let id = event_row_id(calendar_id, &row.uid, row.recurrence_id);
        let rdate_json =
            serde_json::to_string(&row.rdate.iter().map(|d| d.timestamp()).collect::<Vec<_>>())
                .unwrap_or_else(|_| "[]".into());
        let exdate_json =
            serde_json::to_string(&row.exdate.iter().map(|d| d.timestamp()).collect::<Vec<_>>())
                .unwrap_or_else(|_| "[]".into());
        let attendees_json = serde_json::to_string(&row.attendees).unwrap_or_else(|_| "[]".into());
        let reminders_json = serde_json::to_string(&row.reminders).unwrap_or_else(|_| "[]".into());
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO calendar_events
                (id, calendar_id, uid, href, etag, summary, description,
                 start_utc, end_utc, location, rrule, rdate_json, exdate_json,
                 recurrence_id, ics_raw, cached_at,
                 url, transparency, attendees_json, reminders_json,
                 latitude, longitude)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
             ON CONFLICT (id) DO UPDATE SET
                href           = excluded.href,
                etag           = excluded.etag,
                summary        = excluded.summary,
                description    = excluded.description,
                start_utc      = excluded.start_utc,
                end_utc        = excluded.end_utc,
                location       = excluded.location,
                rrule          = excluded.rrule,
                rdate_json     = excluded.rdate_json,
                exdate_json    = excluded.exdate_json,
                recurrence_id  = excluded.recurrence_id,
                ics_raw        = excluded.ics_raw,
                cached_at      = excluded.cached_at,
                url            = excluded.url,
                transparency   = excluded.transparency,
                attendees_json = excluded.attendees_json,
                reminders_json = excluded.reminders_json,
                latitude       = excluded.latitude,
                longitude      = excluded.longitude",
            params![
                id,
                calendar_id,
                row.uid,
                row.href,
                row.etag,
                row.summary,
                row.description,
                row.start.timestamp(),
                row.end.timestamp(),
                row.location,
                row.rrule,
                rdate_json,
                exdate_json,
                row.recurrence_id.map(|d| d.timestamp()),
                row.ics_raw,
                now,
                row.url,
                row.transparency,
                attendees_json,
                reminders_json,
                row.latitude,
                row.longitude,
            ],
        )?;
        Ok(())
    }

    /// Persist the user's RSVP answer (ACCEPTED / DECLINED / TENTATIVE)
    /// for the iCalendar UID of an inbound invite. Called from the
    /// `send_event_rsvp` Tauri command after the REPLY mail leaves
    /// the SMTP server, so the card can re-render in the chosen
    /// state on next open.  Overwrites a previous answer, which is
    /// the right semantics: changing your mind replaces the row.
    pub fn upsert_rsvp_response(&self, uid: &str, partstat: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO rsvp_responses (uid, partstat, responded_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (uid) DO UPDATE SET
                partstat     = excluded.partstat,
                responded_at = excluded.responded_at",
            params![uid, partstat, now],
        )?;
        Ok(())
    }

    /// Look up the user's last RSVP answer for `uid`. Returns
    /// `Ok(None)` if the user has never answered this invite (the
    /// card should show the fresh Accept/Decline/Tentative state).
    pub fn get_rsvp_response(&self, uid: &str) -> Result<Option<String>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT partstat FROM rsvp_responses WHERE uid = ?1",
                params![uid],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// Record that the iCalendar UID has been cancelled by the
    /// organiser.  Called by MailView when it opens a
    /// `METHOD:CANCEL` invite — the original REQUEST mail's
    /// pre-render sync then sees this signal and flips its card
    /// to the cancelled banner.  Idempotent: re-recording the
    /// same UID just refreshes `cancelled_at`.
    pub fn mark_invite_cancelled(&self, uid: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO cancelled_invites (uid, cancelled_at)
             VALUES (?1, ?2)
             ON CONFLICT (uid) DO UPDATE SET cancelled_at = excluded.cancelled_at",
            params![uid, now],
        )?;
        Ok(())
    }

    /// True when a `METHOD:CANCEL` for this UID has been seen
    /// in the user's inbox at any point.  Used to flip the
    /// REQUEST mail's RSVP card to the cancelled banner so the
    /// user doesn't unwittingly respond to an already-cancelled
    /// meeting.
    pub fn is_invite_cancelled(&self, uid: &str) -> Result<bool, CacheError> {
        let conn = self.conn()?;
        let hit: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM cancelled_invites WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Find the first cached event whose iCalendar UID matches.
    /// Used by the iMIP CANCEL flow: when an organiser cancels a
    /// meeting we previously accepted, the inbound mail only carries
    /// the UID — we need to map that back to the local event so we
    /// can remove it from the user's calendar.
    pub fn find_event_id_by_uid(&self, uid: &str) -> Result<Option<String>, CacheError> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT id FROM calendar_events WHERE uid = ?1 LIMIT 1",
                params![uid],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// Remove one event row by its app-side id. Used after a successful
    /// DELETE to the server, so the next `get_cached_events` call
    /// doesn't ghost-render the deleted row until the next sync.
    pub fn delete_event_by_id(&self, event_id: &str) -> Result<(), CacheError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM calendar_events WHERE id = ?1",
            params![event_id],
        )?;
        Ok(())
    }
}

/// Server-side bookkeeping for one cached event, returned from
/// [`Cache::get_event_server_handle`]. The Tauri layer needs these
/// fields to do a PUT or DELETE — the user-facing `CalendarEvent`
/// hides them since the UI shouldn't touch hrefs and etags.
#[derive(Debug, Clone)]
pub struct CalendarEventServerHandle {
    pub nextcloud_account_id: String,
    pub calendar_id: String,
    /// Absolute URL of the calendar collection (already includes
    /// scheme + host — see [`CalendarRow::path`]).
    pub calendar_path: String,
    pub uid: String,
    pub href: String,
    pub etag: String,
    pub recurrence_id: Option<DateTime<Utc>>,
    /// The full text/calendar blob as the server last sent it. Useful
    /// for write paths that want to preserve fields the editor doesn't
    /// surface yet.
    pub ics_raw: String,
}

/// Build the stable app-side event id.
///
/// Matches the schema comment: `{calendar_id}::{uid}` for a master
/// (or a non-recurring VEVENT), and `{calendar_id}::{uid}::{epoch}`
/// for a RECURRENCE-ID override. Callers use this id as their single
/// handle to a row.
fn event_row_id(calendar_id: &str, uid: &str, recurrence_id: Option<DateTime<Utc>>) -> String {
    match recurrence_id {
        Some(t) => format!("{calendar_id}::{uid}::{}", t.timestamp()),
        None => format!("{calendar_id}::{uid}"),
    }
}

/// Column list the three event queries select in lockstep so
/// `row_to_calendar_event` can address them by index without drifting
/// out of sync with individual `SELECT` statements.
const EVENT_COLUMNS: &str = "id, calendar_id, summary, description, start_utc, end_utc, \
     location, rrule, rdate_json, exdate_json, recurrence_id, \
     url, transparency, attendees_json, reminders_json, latitude, longitude";

/// `?1, ?2, ?N` placeholder list for binding a slice of calendar ids
/// into an `IN (…)` clause. `n` is the number of calendar ids the
/// caller has — the result is always safe because the string only
/// contains `?` and digits, never user input.
fn sql_placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Shared row → `CalendarEvent` mapper. All three event queries keep
/// their `SELECT` list equal to [`EVENT_COLUMNS`] so the column
/// indices here are stable.
fn row_to_calendar_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let start_ts: i64 = r.get(4)?;
    let end_ts: i64 = r.get(5)?;
    let recurrence_ts: Option<i64> = r.get(10)?;
    let rdate_json: String = r.get(8)?;
    let exdate_json: String = r.get(9)?;
    let attendees_json: String = r.get(13)?;
    let reminders_json: String = r.get(14)?;
    let rdate_epochs: Vec<i64> = serde_json::from_str(&rdate_json).unwrap_or_default();
    let exdate_epochs: Vec<i64> = serde_json::from_str(&exdate_json).unwrap_or_default();
    let attendees: Vec<EventAttendee> = serde_json::from_str(&attendees_json).unwrap_or_default();
    let reminders: Vec<EventReminder> = serde_json::from_str(&reminders_json).unwrap_or_default();
    Ok(CalendarEvent {
        id: r.get(0)?,
        summary: r.get(2)?,
        description: r.get(3)?,
        start: Utc
            .timestamp_opt(start_ts, 0)
            .single()
            .unwrap_or_else(Utc::now),
        end: Utc
            .timestamp_opt(end_ts, 0)
            .single()
            .unwrap_or_else(Utc::now),
        location: r.get(6)?,
        rrule: r.get(7)?,
        rdate: rdate_epochs
            .into_iter()
            .filter_map(|t| Utc.timestamp_opt(t, 0).single())
            .collect(),
        exdate: exdate_epochs
            .into_iter()
            .filter_map(|t| Utc.timestamp_opt(t, 0).single())
            .collect(),
        recurrence_id: recurrence_ts.and_then(|t| Utc.timestamp_opt(t, 0).single()),
        url: r.get(11)?,
        transparency: r.get(12)?,
        attendees,
        reminders,
        latitude: r.get(15)?,
        longitude: r.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn open_test_cache() -> Cache {
        Cache::open_in_memory().expect("open in-memory cache")
    }

    fn cal(path: &str, name: &str) -> CalendarRow {
        CalendarRow {
            path: path.into(),
            display_name: name.into(),
            color: Some("#2bb0ed".into()),
            ctag: Some(format!("ctag-{path}")),
            hidden: false,
            muted: false,
            read_only: false,
        }
    }

    fn event(uid: &str, href: &str, offset_hours: i64) -> CalendarEventRow {
        let start = Utc::now() + Duration::hours(offset_hours);
        CalendarEventRow {
            uid: uid.into(),
            recurrence_id: None,
            href: href.into(),
            etag: format!("etag-{uid}"),
            summary: format!("Event {uid}"),
            description: None,
            start,
            end: start + Duration::hours(1),
            location: Some("Somewhere".into()),
            rrule: None,
            rdate: vec![],
            exdate: vec![],
            url: None,
            transparency: None,
            attendees: vec![],
            reminders: vec![],
            latitude: None,
            longitude: None,
            ics_raw: format!("BEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\n"),
        }
    }

    #[test]
    fn upsert_and_list_calendars() {
        let cache = open_test_cache();
        cache
            .upsert_calendars(
                "nc1",
                &[cal("/dav/personal/", "Personal"), cal("/dav/work/", "Work")],
            )
            .unwrap();

        let got = cache.list_calendars("nc1").unwrap();
        assert_eq!(got.len(), 2);
        // Alphabetical by display_name.
        assert_eq!(got[0].display_name, "Personal");
        assert_eq!(got[1].display_name, "Work");
        // id derived from (nc_id, path).
        assert_eq!(got[0].id, "nc1::/dav/personal/");
        // sync_token starts empty — only a real sync sets it.
        assert!(got[0].sync_token.is_none());
    }

    #[test]
    fn upsert_preserves_sync_token_but_updates_name_and_ctag() {
        let cache = open_test_cache();
        cache
            .upsert_calendars("nc1", &[cal("/dav/personal/", "Personal")])
            .unwrap();
        let id = "nc1::/dav/personal/".to_string();

        // Pretend we've synced once — writes sync_token + ctag + last_synced_at.
        cache
            .apply_event_delta(&id, &[], &[], Some("tok-1"), Some("ctag-after-sync"))
            .unwrap();

        // Now re-discover with a new display_name and ctag.
        cache
            .upsert_calendars(
                "nc1",
                &[CalendarRow {
                    display_name: "Personal (renamed)".into(),
                    ctag: Some("ctag-v2".into()),
                    ..cal("/dav/personal/", "Personal")
                }],
            )
            .unwrap();

        let got = cache.list_calendars("nc1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display_name, "Personal (renamed)");
        // ctag updated…
        assert_eq!(got[0].ctag.as_deref(), Some("ctag-v2"));
        // …but the sync_token survived (discovery can't know it).
        assert_eq!(got[0].sync_token.as_deref(), Some("tok-1"));
    }

    #[test]
    fn upsert_prunes_missing_calendars_and_cascades_events() {
        let cache = open_test_cache();
        cache
            .upsert_calendars(
                "nc1",
                &[cal("/dav/personal/", "Personal"), cal("/dav/work/", "Work")],
            )
            .unwrap();

        let personal = "nc1::/dav/personal/".to_string();
        cache
            .apply_event_delta(
                &personal,
                &[event("u1", "/dav/personal/u1.ics", 1)],
                &[],
                Some("tok"),
                None,
            )
            .unwrap();
        assert_eq!(
            cache
                .list_events_in_range(
                    std::slice::from_ref(&personal),
                    Utc::now() - Duration::hours(1),
                    Utc::now() + Duration::hours(5),
                )
                .unwrap()
                .len(),
            1
        );

        // Second discovery: only Work remains.
        cache
            .upsert_calendars("nc1", &[cal("/dav/work/", "Work")])
            .unwrap();

        // Personal is gone…
        assert_eq!(cache.list_calendars("nc1").unwrap().len(), 1);
        // …and its events with it (FK cascade).
        let still_there = cache
            .list_events_in_range(
                &[personal],
                Utc::now() - Duration::hours(1),
                Utc::now() + Duration::hours(5),
            )
            .unwrap();
        assert!(still_there.is_empty());
    }

    #[test]
    fn apply_delta_upserts_and_deletes() {
        let cache = open_test_cache();
        cache
            .upsert_calendars("nc1", &[cal("/dav/personal/", "Personal")])
            .unwrap();
        let cal_id = "nc1::/dav/personal/".to_string();

        cache
            .apply_event_delta(
                &cal_id,
                &[event("u1", "/dav/u1.ics", 1), event("u2", "/dav/u2.ics", 2)],
                &[],
                Some("tok-1"),
                Some("ctag-1"),
            )
            .unwrap();

        let got = cache
            .list_events_in_range(
                std::slice::from_ref(&cal_id),
                Utc::now() - Duration::hours(1),
                Utc::now() + Duration::hours(10),
            )
            .unwrap();
        assert_eq!(got.len(), 2);

        // Delete u1 via href.
        cache
            .apply_event_delta(&cal_id, &[], &["/dav/u1.ics".into()], Some("tok-2"), None)
            .unwrap();

        let got = cache
            .list_events_in_range(
                std::slice::from_ref(&cal_id),
                Utc::now() - Duration::hours(1),
                Utc::now() + Duration::hours(10),
            )
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].summary, "Event u2");

        // Sync bookmark stuck around and bumped forward.
        let s = cache.get_calendar_sync_state(&cal_id).unwrap().unwrap();
        assert_eq!(s.sync_token.as_deref(), Some("tok-2"));
        // ctag was COALESCEd — the second call passed None, so the
        // existing "ctag-1" must survive.
        assert_eq!(s.ctag.as_deref(), Some("ctag-1"));
        assert!(s.last_synced_at.is_some());
    }

    #[test]
    fn master_and_override_coexist_distinct_ids() {
        let cache = open_test_cache();
        cache
            .upsert_calendars("nc1", &[cal("/dav/personal/", "Personal")])
            .unwrap();
        let cal_id = "nc1::/dav/personal/".to_string();

        let master = event("weekly", "/dav/weekly.ics", 1);
        let override_start = master.start + Duration::days(7);
        let override_row = CalendarEventRow {
            recurrence_id: Some(override_start),
            start: override_start + Duration::hours(2), // moved later
            end: override_start + Duration::hours(3),
            summary: "Event weekly (moved)".into(),
            ..event("weekly", "/dav/weekly.ics", 0)
        };

        cache
            .apply_event_delta(&cal_id, &[master, override_row], &[], Some("tok"), None)
            .unwrap();

        let got = cache
            .list_events_in_range(
                std::slice::from_ref(&cal_id),
                Utc::now() - Duration::hours(1),
                Utc::now() + Duration::days(14),
            )
            .unwrap();
        assert_eq!(got.len(), 2);
        // Master has no recurrence_id; override does.
        let has_master = got.iter().any(|e| e.recurrence_id.is_none());
        let has_override = got.iter().any(|e| e.recurrence_id.is_some());
        assert!(has_master && has_override);
        // Distinct app-side ids so the UI can address them separately.
        let ids: Vec<&str> = got.iter().map(|e| e.id.as_str()).collect();
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn re_syncing_an_href_drops_removed_overrides() {
        // If the server removes an override but keeps the master,
        // it just sends the updated ICS — no `deleted_hrefs` entry.
        // Our wipe-on-upsert-per-href keeps that consistent.
        let cache = open_test_cache();
        cache
            .upsert_calendars("nc1", &[cal("/dav/personal/", "Personal")])
            .unwrap();
        let cal_id = "nc1::/dav/personal/".to_string();

        let master = event("weekly", "/dav/weekly.ics", 1);
        let override_start = master.start + Duration::days(7);
        let override_row = CalendarEventRow {
            recurrence_id: Some(override_start),
            start: override_start + Duration::hours(2),
            end: override_start + Duration::hours(3),
            ..event("weekly", "/dav/weekly.ics", 0)
        };

        cache
            .apply_event_delta(&cal_id, &[master.clone(), override_row], &[], None, None)
            .unwrap();
        assert_eq!(
            cache
                .list_events_in_range(
                    std::slice::from_ref(&cal_id),
                    Utc::now() - Duration::hours(1),
                    Utc::now() + Duration::days(14),
                )
                .unwrap()
                .len(),
            2
        );

        // Re-sync the same href — just the master this time.
        cache
            .apply_event_delta(&cal_id, &[master], &[], None, None)
            .unwrap();

        let got = cache
            .list_events_in_range(
                &[cal_id],
                Utc::now() - Duration::hours(1),
                Utc::now() + Duration::days(14),
            )
            .unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].recurrence_id.is_none());
    }

    #[test]
    fn range_filter_is_half_open_and_includes_ongoing() {
        let cache = open_test_cache();
        cache
            .upsert_calendars("nc1", &[cal("/dav/personal/", "Personal")])
            .unwrap();
        let cal_id = "nc1::/dav/personal/".to_string();

        let now = Utc::now();
        let mut in_window = event("in", "/dav/in.ics", 0);
        in_window.start = now + Duration::hours(2);
        in_window.end = now + Duration::hours(3);

        let mut straddling = event("straddle", "/dav/s.ics", 0);
        straddling.start = now - Duration::hours(2);
        straddling.end = now + Duration::hours(2);

        let mut before = event("before", "/dav/b.ics", 0);
        before.start = now - Duration::hours(5);
        before.end = now - Duration::hours(4);

        let mut after = event("after", "/dav/a.ics", 0);
        after.start = now + Duration::hours(10);
        after.end = now + Duration::hours(11);

        cache
            .apply_event_delta(
                &cal_id,
                &[in_window, straddling, before, after],
                &[],
                None,
                None,
            )
            .unwrap();

        let got = cache
            .list_events_in_range(
                &[cal_id],
                now - Duration::hours(1),
                now + Duration::hours(5),
            )
            .unwrap();
        let summaries: Vec<&str> = got.iter().map(|e| e.summary.as_str()).collect();
        // Ongoing one pulled in, out-of-window ones excluded.
        assert!(summaries.contains(&"Event in"));
        assert!(summaries.contains(&"Event straddle"));
        assert!(!summaries.contains(&"Event before"));
        assert!(!summaries.contains(&"Event after"));
    }

    #[test]
    fn list_events_for_expansion_splits_rows_correctly() {
        // Three distinct row kinds co-located in one calendar. We want
        // the expansion loader to bucket them correctly: singletons
        // only when in-window, masters and overrides regardless of
        // window.
        let cache = open_test_cache();
        cache
            .upsert_calendars("nc1", &[cal("/dav/personal/", "Personal")])
            .unwrap();
        let cal_id = "nc1::/dav/personal/".to_string();

        let now = Utc::now();
        // (a) In-window singleton.
        let mut singleton = event("single", "/dav/single.ics", 0);
        singleton.start = now + Duration::hours(2);
        singleton.end = now + Duration::hours(3);

        // (b) Recurring master whose first instance is *before* our
        //     window — expansion should still need to see it.
        let mut master = event("weekly", "/dav/weekly.ics", 0);
        master.start = now - Duration::days(60);
        master.end = now - Duration::days(60) + Duration::hours(1);
        master.rrule = Some("FREQ=WEEKLY".into());

        // (c) Override on that series, recurrence_id also outside our
        //     window, but whose new start pushes it into view.
        let override_rid = now + Duration::days(30);
        let override_row = CalendarEventRow {
            recurrence_id: Some(override_rid),
            start: now + Duration::hours(6),
            end: now + Duration::hours(7),
            ..event("weekly", "/dav/weekly.ics", 0)
        };

        // (d) Out-of-window singleton — should NOT show up.
        let mut stale = event("stale", "/dav/stale.ics", 0);
        stale.start = now - Duration::days(10);
        stale.end = now - Duration::days(10) + Duration::hours(1);

        cache
            .apply_event_delta(
                &cal_id,
                &[singleton, master, override_row, stale],
                &[],
                None,
                None,
            )
            .unwrap();

        let input = cache
            .list_events_for_expansion(
                std::slice::from_ref(&cal_id),
                now - Duration::hours(1),
                now + Duration::days(7),
            )
            .unwrap();

        assert_eq!(input.singletons.len(), 1);
        assert_eq!(input.singletons[0].summary, "Event single");
        assert_eq!(input.masters.len(), 1);
        assert!(input.masters[0].rrule.is_some());
        assert_eq!(input.overrides.len(), 1);
        // DB stores timestamps at second precision — compare via epoch
        // rather than the full `DateTime<Utc>` to avoid the nanosecond
        // part of `Utc::now()` leaking into the assertion.
        assert_eq!(
            input.overrides[0].recurrence_id.map(|d| d.timestamp()),
            Some(override_rid.timestamp())
        );
    }

    #[test]
    fn wipe_nextcloud_calendars_clears_everything() {
        let cache = open_test_cache();
        cache
            .upsert_calendars(
                "nc1",
                &[cal("/dav/personal/", "Personal"), cal("/dav/work/", "Work")],
            )
            .unwrap();
        let cal_id = "nc1::/dav/personal/".to_string();
        cache
            .apply_event_delta(
                &cal_id,
                &[event("u1", "/dav/u1.ics", 1)],
                &[],
                Some("tok"),
                None,
            )
            .unwrap();

        cache.wipe_nextcloud_calendars("nc1").unwrap();

        assert!(cache.list_calendars("nc1").unwrap().is_empty());
        assert!(
            cache
                .list_events_in_range(
                    &[cal_id],
                    Utc::now() - Duration::hours(1),
                    Utc::now() + Duration::hours(5),
                )
                .unwrap()
                .is_empty()
        );
    }
}
